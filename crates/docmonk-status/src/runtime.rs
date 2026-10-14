//! Run liveness: which source passes are running, on which host, and
//! whether they are still alive.
//!
//! * **Local mode**: this home's progress snapshot (one entry per source
//!   pass), its locks and a process probe. A pass is `live` while its
//!   process exists and its heartbeat is fresh; a snapshot that says
//!   running for a dead process is a crash.
//! * **Server mode**: `{prefix}-runtime` heartbeat documents of every
//!   host, checked against the source's writer lease (fencing token). A
//!   remote PID is never used as proof.

use chrono::{DateTime, Utc};

use crate::model::{LastRun, LiveRun, Liveness, LockInfo};
use crate::snapshot::{age_seconds, normalize_time, percentage};

/// Facts about this home's local runs.
#[derive(Debug, Clone, Default)]
pub struct LocalRuntime {
    pub runs: Vec<LiveRun>,
    pub lock: Option<LockInfo>,
    pub last_run: Option<LastRun>,
    pub max_parallel_sources: Option<u64>,
    pub resources: Option<serde_json::Value>,
}

fn count(v: i64) -> Option<u64> {
    u64::try_from(v).ok()
}

/// A lock as the report shows it.
pub fn lock_info(state: &docmonk_indexing::lock::LockState) -> LockInfo {
    use docmonk_indexing::lock::LockState;
    let (name, owner) = match state {
        LockState::Free => ("free", None),
        LockState::Held(o) => ("held", o.as_ref()),
        LockState::Unknown => ("unknown", None),
    };
    LockInfo {
        state: name.into(),
        pid: owner.and_then(|o| o.pid.as_deref()?.parse().ok()),
        operation: owner.and_then(|o| o.operation.clone()),
        hostname: owner.and_then(|o| o.hostname.clone()),
        source_id: owner.and_then(|o| o.source_id.clone()),
        acquired_at: owner.and_then(|o| normalize_time(o.acquired_at.as_deref())),
    }
}

/// Judges local runs from plain inputs (pure; `alive` probes a PID).
/// `source_locks` lists `(source_id, lock)` of sources whose lock is held.
pub fn local_runs(
    snapshot: Option<&docmonk_indexing::progress::IndexProgress>,
    home_lock: LockInfo,
    source_locks: &[(String, LockInfo)],
    alive: &dyn Fn(i64) -> bool,
    threshold: f64,
    now: DateTime<Utc>,
) -> LocalRuntime {
    let mut out = LocalRuntime {
        lock: Some(home_lock),
        ..LocalRuntime::default()
    };
    if let Some(p) = snapshot {
        out.max_parallel_sources = p.max_parallel_sources.and_then(count);
        let process_alive = p.running && p.pid.is_some_and(alive);
        if p.running && process_alive {
            out.resources = p.resources.clone();
        }
        for s in &p.sources {
            let heartbeat = normalize_time(s.heartbeat_at.as_deref().or(p.updated_at.as_deref()));
            let age = age_seconds(heartbeat.as_deref(), now);
            let liveness = if !s.active {
                Liveness::Finished
            } else if !p.running || !process_alive {
                Liveness::Expired
            } else if age.is_some_and(|a| a > threshold) {
                Liveness::Stalled
            } else {
                Liveness::Live
            };
            let processed = count(s.processed);
            let planned = s.planned.and_then(count);
            out.runs.push(LiveRun {
                run_id: p.run_id.clone(),
                source_id: s.source_id.clone(),
                host: p.host.clone(),
                pid: p.pid,
                operation: p.operation.clone(),
                stage: s.stage.clone(),
                liveness,
                scanned: count(s.scanned),
                planned,
                processed,
                indexed: count(s.indexed),
                failed: count(s.failed),
                retry: count(s.retry),
                percentage: percentage(processed, planned),
                started_at: normalize_time(s.started_at.as_deref()),
                last_progress_at: normalize_time(s.last_progress_at.as_deref()),
                heartbeat_at: heartbeat,
                heartbeat_age_seconds: age,
                lease_token: None,
                lease_valid: None,
                outcome: s.outcome.clone(),
                relationships: (s.relationships.as_deref() == Some("building")).then(|| {
                    crate::model::RelationshipProgress {
                        phase: s.relationships_phase.clone(),
                        processed: count(s.relationships_processed),
                        planned: s.relationships_planned.and_then(count),
                    }
                }),
            });
        }
        if !p.running {
            out.last_run = Some(LastRun {
                run_id: p.run_id.clone(),
                operation: p.operation.clone(),
                outcome: p.outcome.clone(),
                started_at: normalize_time(p.started_at.as_deref()),
                completed_at: normalize_time(p.completed_at.as_deref()),
                error: p.error.clone(),
            });
        } else if !process_alive {
            out.last_run = Some(LastRun {
                run_id: p.run_id.clone(),
                operation: p.operation.clone(),
                outcome: "crashed".into(),
                started_at: normalize_time(p.started_at.as_deref()),
                completed_at: None,
                error: None,
            });
        }
    }
    // A held source lock without a live progress entry: another process
    // in this home runs a pass whose progress is not in this snapshot.
    for (source_id, lock) in source_locks {
        let shown = out
            .runs
            .iter()
            .any(|r| &r.source_id == source_id && r.liveness == Liveness::Live);
        if shown || lock.state != "held" {
            continue;
        }
        out.runs.retain(|r| &r.source_id != source_id);
        out.runs.push(LiveRun {
            run_id: None,
            source_id: source_id.clone(),
            host: lock.hostname.clone(),
            pid: lock.pid,
            operation: lock.operation.clone(),
            stage: None,
            liveness: Liveness::Live,
            scanned: None,
            planned: None,
            processed: None,
            indexed: None,
            failed: None,
            retry: None,
            percentage: None,
            started_at: lock.acquired_at.clone(),
            last_progress_at: None,
            heartbeat_at: None,
            heartbeat_age_seconds: None,
            lease_token: None,
            lease_valid: None,
            outcome: None,
            relationships: None,
        });
    }
    out
}

/// [`local_runs`] over the real files of `home`.
pub fn read_local(
    home: &docmonk_core::paths::Home,
    source_ids: &[String],
    threshold: f64,
    now: DateTime<Utc>,
) -> LocalRuntime {
    use docmonk_indexing::lock::{inspect_lock, source_lock_path};
    let snapshot = docmonk_indexing::progress::read_progress(&home.index_progress());
    let home_lock = lock_info(&inspect_lock(&home.locks_dir().join("index.lock")));
    let source_locks: Vec<(String, LockInfo)> = source_ids
        .iter()
        .filter_map(|id| {
            let path = source_lock_path(home, id);
            path.exists()
                .then(|| (id.clone(), lock_info(&inspect_lock(&path))))
        })
        .collect();
    local_runs(
        snapshot.as_ref(),
        home_lock,
        &source_locks,
        &docmonk_indexing::runtime::is_process_alive,
        threshold,
        now,
    )
}

/// Finished runs older than this are no longer shown.
pub const FINISHED_RETENTION_SECONDS: f64 = 3600.0;

fn str_field<'a>(d: &'a serde_json::Value, k: &str) -> Option<&'a str> {
    d.get(k).and_then(serde_json::Value::as_str)
}

fn num_field(d: &serde_json::Value, k: &str) -> Option<u64> {
    d.get(k).and_then(|v| {
        v.as_u64()
            .or_else(|| v.as_i64().and_then(|n| u64::try_from(n).ok()))
    })
}

/// Server runs: heartbeat documents judged against the sources' writer
/// leases. A run is `live` only while its lease (owner and fencing token)
/// is still the source's live lease, its document has not expired and its
/// heartbeat is fresh; an expired or superseded run is never running.
pub fn server_runs(
    docs: &[serde_json::Value],
    leases: &std::collections::BTreeMap<String, crate::snapshot::LeaseFacts>,
    threshold: f64,
    now: DateTime<Utc>,
) -> Vec<LiveRun> {
    use crate::snapshot::parse_time;
    let mut out = Vec::new();
    for d in docs {
        let Some(source_id) = str_field(d, "source_id") else {
            continue;
        };
        let heartbeat = normalize_time(str_field(d, "heartbeat_at"));
        let age = age_seconds(heartbeat.as_deref(), now);
        let token = d.get("lease_token").and_then(serde_json::Value::as_i64);
        let owner = str_field(d, "owner");
        let lease = leases.get(source_id);
        let lease_valid = lease
            .is_some_and(|l| l.live && Some(l.owner.as_str()) == owner && Some(l.token) == token);
        let finished = d.get("active").and_then(serde_json::Value::as_bool) == Some(false)
            || str_field(d, "outcome").is_some();
        let expired = str_field(d, "expires_at")
            .and_then(parse_time)
            .is_none_or(|e| e <= now);
        let liveness = if finished {
            Liveness::Finished
        } else if expired || !lease_valid {
            Liveness::Expired
        } else if age.is_some_and(|a| a > threshold) {
            Liveness::Stalled
        } else {
            Liveness::Live
        };
        if liveness == Liveness::Finished {
            let ended =
                normalize_time(str_field(d, "finished_at").or(str_field(d, "heartbeat_at")));
            if age_seconds(ended.as_deref(), now).is_none_or(|a| a > FINISHED_RETENTION_SECONDS) {
                continue;
            }
        }
        let processed = num_field(d, "processed");
        let planned = num_field(d, "planned");
        out.push(LiveRun {
            run_id: str_field(d, "run_id").map(str::to_owned),
            source_id: source_id.to_owned(),
            host: str_field(d, "host").map(str::to_owned),
            pid: None,
            operation: str_field(d, "operation").map(str::to_owned),
            stage: str_field(d, "stage").map(str::to_owned),
            liveness,
            scanned: num_field(d, "scanned"),
            planned,
            processed,
            indexed: num_field(d, "indexed"),
            failed: num_field(d, "failed"),
            retry: num_field(d, "retry"),
            percentage: percentage(processed, planned),
            started_at: normalize_time(str_field(d, "started_at")),
            last_progress_at: normalize_time(str_field(d, "last_progress_at")),
            heartbeat_at: heartbeat,
            heartbeat_age_seconds: age,
            lease_token: token,
            lease_valid: Some(lease_valid),
            outcome: str_field(d, "outcome").map(str::to_owned),
            relationships: None,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{iso, LeaseFacts};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn ms(t: DateTime<Utc>) -> String {
        t.timestamp_millis().to_string()
    }

    #[test]
    fn server_liveness_requires_a_valid_lease_and_fresh_heartbeat() {
        let now = Utc::now();
        let doc = |src: &str, owner: &str, token: i64, beat: i64, exp: i64| {
            json!({
                "source_id": src, "run_id": "r", "host": owner.split(':').next(),
                "owner": owner, "lease_token": token, "stage": "processing",
                "active": true, "processed": 3, "planned": 6,
                "heartbeat_at": ms(now - chrono::Duration::seconds(beat)),
                "expires_at": ms(now + chrono::Duration::seconds(exp)),
            })
        };
        let lease = |owner: &str, token: i64, live: bool| LeaseFacts {
            owner: owner.into(),
            token,
            expires_at: Some(iso(now)),
            live,
        };
        let mut leases = BTreeMap::new();
        leases.insert("live".to_owned(), lease("a:1", 4, true));
        leases.insert("superseded".to_owned(), lease("b:2", 9, true));
        leases.insert("lease_gone".to_owned(), lease("a:1", 4, false));
        leases.insert("stale".to_owned(), lease("a:1", 4, true));
        leases.insert("expired_doc".to_owned(), lease("a:1", 4, true));
        let docs = vec![
            doc("live", "a:1", 4, 1, 60),
            doc("superseded", "a:1", 4, 1, 60),
            doc("lease_gone", "a:1", 4, 1, 60),
            doc("stale", "a:1", 4, 500, 60),
            doc("expired_doc", "a:1", 4, 1, -5),
            json!({"source_id": "done", "active": false, "outcome": "completed",
                   "finished_at": ms(now), "heartbeat_at": ms(now)}),
            json!({"source_id": "old", "active": false, "outcome": "completed",
                   "finished_at": ms(now - chrono::Duration::hours(5))}),
        ];
        let runs = server_runs(&docs, &leases, 120.0, now);
        let got: Vec<(&str, Liveness)> = runs
            .iter()
            .map(|r| (r.source_id.as_str(), r.liveness))
            .collect();
        assert_eq!(
            got,
            [
                ("live", Liveness::Live),
                ("superseded", Liveness::Expired),
                ("lease_gone", Liveness::Expired),
                ("stale", Liveness::Stalled),
                ("expired_doc", Liveness::Expired),
                ("done", Liveness::Finished),
            ]
        );
        assert!(
            runs.iter().all(|r| r.pid.is_none()),
            "remote PIDs are never shown"
        );
        assert_eq!(runs[0].percentage, Some(50.0));
        assert_eq!(runs[0].host.as_deref(), Some("a"));
    }
}
