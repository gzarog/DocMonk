//! Golden verdicts of the one health evaluator, through the one report
//! builder, with a fixed clock. The same facts give the same verdict in
//! local and server mode.

use chrono::{DateTime, Utc};
use docmonk_status::model::*;
use docmonk_status::snapshot::{assemble, CollectOptions, Collected, SourceFacts};

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-08T15:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn facts(id: &str) -> SourceFacts {
    SourceFacts {
        source_id: id.into(),
        path: format!("/srv/{id}"),
        source_type: "local".into(),
        enabled: true,
        access: Access::Online,
        build_state: BuildState::Ready,
        active_build_id: Some(format!("b_{id}")),
        pending_build_id: None,
        published_at: Some("2026-10-08T10:00:00.000000Z".into()),
        last_scan_at: Some("2026-10-08T10:00:00.000000Z".into()),
        last_error: None,
        last_error_at: None,
        published: Some(PublishedCounts {
            files: 10,
            indexed: 10,
            ..PublishedCounts::default()
        }),
        published_missing: false,
        lease: None,
        relationships: None,
        dependencies: None,
    }
}

fn run(id: &str, stage: &str, liveness: Liveness) -> LiveRun {
    LiveRun {
        run_id: Some("run-1".into()),
        source_id: id.into(),
        host: Some("host-a".into()),
        pid: None,
        operation: Some("index".into()),
        stage: Some(stage.into()),
        liveness,
        scanned: Some(10),
        planned: Some(4),
        processed: Some(1),
        indexed: Some(1),
        failed: Some(0),
        retry: Some(0),
        percentage: Some(25.0),
        started_at: None,
        last_progress_at: None,
        heartbeat_at: None,
        heartbeat_age_seconds: Some(if liveness == Liveness::Stalled {
            600.0
        } else {
            1.0
        }),
        lease_token: Some(1),
        lease_valid: Some(liveness == Liveness::Live),
        outcome: None,
        relationships: None,
    }
}

fn collected(mode: Mode, sources: Vec<SourceFacts>, runs: Vec<LiveRun>) -> Collected {
    let (kind, cluster) = match mode {
        Mode::Local => (BackendKind::Sqlite, None),
        Mode::Server => (
            BackendKind::Opensearch,
            Some(ClusterInfo {
                health: Some(ClusterHealth::Green),
                unassigned_shards: Some(0),
                number_of_nodes: Some(3),
                indexes: vec![],
            }),
        ),
    };
    Collected {
        mode,
        backend: BackendInfo {
            kind,
            index_prefix: None,
            endpoint: None,
            reachable: true,
            authoritative: true,
            cluster,
            latency_ms: None,
        },
        sources,
        runs,
        scope: IndexerScope::Host,
        max_parallel_sources: None,
        resources: None,
        lock: None,
        last_run: None,
        errors: Some(vec![]),
        missing_sections: vec![],
        request_count: None,
        cached_sections: vec![],
        consistency: Consistency::Consistent,
        inconsistent_sources: vec![],
    }
}

fn report(c: Collected) -> StatusReport {
    assemble(c, &CollectOptions::default(), now(), None)
}

fn state_of(f: SourceFacts, runs: Vec<LiveRun>) -> SourceIndexState {
    let id = f.source_id.clone();
    report(collected(Mode::Local, vec![f], runs))
        .source(&id)
        .unwrap()
        .index_state
}

fn codes(r: &StatusReport) -> Vec<ProblemCode> {
    r.problems.iter().map(|p| p.code).collect()
}

#[test]
fn source_state_precedence_table() {
    use SourceIndexState as S;
    let live = |stage| vec![run("s", stage, Liveness::Live)];
    let cases: Vec<(&str, SourceFacts, Vec<LiveRun>, SourceIndexState)> = vec![
        (
            "disabled beats everything",
            SourceFacts {
                enabled: false,
                access: Access::Offline,
                ..facts("s")
            },
            live("processing"),
            S::Disabled,
        ),
        (
            "offline beats a live pass",
            SourceFacts {
                access: Access::Offline,
                ..facts("s")
            },
            live("processing"),
            S::Offline,
        ),
        (
            "starting pass is queued",
            facts("s"),
            live("starting"),
            S::Queued,
        ),
        ("scanning", facts("s"), live("scanning"), S::Scanning),
        (
            "processing is indexing",
            facts("s"),
            live("processing"),
            S::Indexing,
        ),
        ("finalizing", facts("s"), live("finalizing"), S::Finalizing),
        ("publishing", facts("s"), live("publishing"), S::Publishing),
        (
            "stalled pass",
            facts("s"),
            vec![run("s", "processing", Liveness::Stalled)],
            S::Stalled,
        ),
        (
            "expired lease is not running",
            facts("s"),
            vec![run("s", "processing", Liveness::Expired)],
            S::Stalled,
        ),
        (
            "finished pass does not count",
            facts("s"),
            vec![run("s", "done", Liveness::Finished)],
            S::Completed,
        ),
        (
            "unreadable published section",
            SourceFacts {
                published: None,
                published_missing: true,
                ..facts("s")
            },
            vec![],
            S::Unknown,
        ),
        (
            "retry files",
            SourceFacts {
                published: Some(PublishedCounts {
                    retrying: 2,
                    ..PublishedCounts::default()
                }),
                ..facts("s")
            },
            vec![],
            S::Retrying,
        ),
        (
            "failed with nothing published",
            SourceFacts {
                active_build_id: None,
                published: None,
                build_state: BuildState::Failed,
                ..facts("s")
            },
            vec![],
            S::Failed,
        ),
        (
            "failed pending build, previous build serves",
            SourceFacts {
                build_state: BuildState::Failed,
                ..facts("s")
            },
            vec![],
            S::Completed,
        ),
        ("published", facts("s"), vec![], S::Completed),
        (
            "never indexed",
            SourceFacts {
                active_build_id: None,
                published: None,
                build_state: BuildState::NotIndexed,
                ..facts("s")
            },
            vec![],
            S::NotIndexed,
        ),
        (
            "scan finished but nothing published",
            SourceFacts {
                active_build_id: None,
                published: None,
                build_state: BuildState::Building,
                ..facts("s")
            },
            vec![],
            S::NotIndexed,
        ),
        (
            "another source's pass is not this one's",
            facts("s"),
            vec![run("other", "processing", Liveness::Live)],
            S::Completed,
        ),
    ];
    for (name, f, runs, want) in cases {
        assert_eq!(state_of(f, runs), want, "{name}");
    }
    let r = report(collected(
        Mode::Local,
        vec![facts("s")],
        vec![run("s", "done", Liveness::Finished)],
    ));
    assert!(
        r.indexer.workers.is_empty(),
        "finished passes are not workers"
    );
    assert!(
        r.sources[0].live.is_some(),
        "but stay visible on their source"
    );
}

#[test]
fn verdicts_are_identical_in_local_and_server_mode() {
    let sources = vec![
        SourceFacts {
            published: Some(PublishedCounts {
                failed: 2,
                ..PublishedCounts::default()
            }),
            ..facts("a")
        },
        SourceFacts {
            access: Access::Offline,
            last_error: Some("unmounted".into()),
            ..facts("b")
        },
        facts("c"),
    ];
    let runs = vec![run("c", "processing", Liveness::Live)];
    let local = report(collected(Mode::Local, sources.clone(), runs.clone()));
    let server = report(collected(Mode::Server, sources, runs));
    let view = |r: &StatusReport| {
        (
            r.health.state,
            codes(r),
            r.sources.iter().map(|s| s.index_state).collect::<Vec<_>>(),
        )
    };
    assert_eq!(view(&local), view(&server));
    assert_eq!(local.health.state, HealthState::Degraded);
}

#[test]
fn cluster_health_is_never_silently_healthy() {
    let with = |health: Option<ClusterHealth>, nodes: u64| {
        let mut c = collected(Mode::Server, vec![facts("a")], vec![]);
        c.backend.cluster = Some(ClusterInfo {
            health,
            unassigned_shards: Some(5),
            number_of_nodes: Some(nodes),
            indexes: vec![],
        });
        report(c)
    };
    let red = with(Some(ClusterHealth::Red), 3);
    assert_eq!(red.health.state, HealthState::Failed);
    assert_eq!(codes(&red), [ProblemCode::ClusterRed]);
    let yellow = with(Some(ClusterHealth::Yellow), 1);
    assert_eq!(yellow.health.state, HealthState::Degraded);
    let p = &yellow.problems[0];
    assert_eq!(p.code, ProblemCode::ClusterYellow);
    assert!(p.message.contains("5 unassigned"), "{}", p.message);
    assert!(p.hint.as_deref().unwrap().contains("single-node"));
    let unknown = with(None, 1);
    assert_eq!(codes(&unknown), [ProblemCode::ClusterHealthUnknown]);
    assert_eq!(
        with(Some(ClusterHealth::Green), 3).health.state,
        HealthState::Healthy
    );
    let mut c = collected(Mode::Server, vec![facts("a")], vec![]);
    c.backend.reachable = false;
    let down = report(c);
    assert_eq!(down.health.state, HealthState::Failed);
    assert_eq!(codes(&down)[0], ProblemCode::BackendUnreachable);
    assert_eq!(down.exit_code(), 4);
}

#[test]
fn partial_and_inconsistent_snapshots_cannot_be_healthy() {
    let mut c = collected(Mode::Server, vec![facts("a")], vec![]);
    c.missing_sections = vec!["runtime (timeout)".into()];
    let r = report(c);
    assert!(r.diagnostics.partial);
    assert_eq!(r.health.state, HealthState::Unknown);
    let mut c = collected(Mode::Server, vec![facts("a")], vec![]);
    c.consistency = Consistency::Inconsistent;
    c.inconsistent_sources = vec!["a".into()];
    let r = report(c);
    assert_eq!(r.health.state, HealthState::Unknown);
    assert!(codes(&r).contains(&ProblemCode::SnapshotInconsistent));
}

#[test]
fn retries_and_failed_builds_say_exactly_what_they_mean() {
    let retry = SourceFacts {
        published: Some(PublishedCounts {
            retrying: 3,
            next_retry_at: Some("2026-10-08T14:00:00.000000Z".into()),
            ..PublishedCounts::default()
        }),
        ..facts("a")
    };
    let r = report(collected(Mode::Local, vec![retry.clone()], vec![]));
    assert_eq!(
        r.indexer.active_source_count, 0,
        "retries are not a running pass"
    );
    let p = r
        .problems
        .iter()
        .find(|p| p.code == ProblemCode::FilesRetrying)
        .unwrap();
    assert!(p
        .hint
        .as_deref()
        .unwrap()
        .contains("does not mean a pass is running"));
    // Due and nobody running: an info nudge, which never degrades health.
    assert!(codes(&r).contains(&ProblemCode::PendingWithoutIndexer));
    let r = report(collected(
        Mode::Local,
        vec![retry],
        vec![run("x", "scanning", Liveness::Live)],
    ));
    assert!(!codes(&r).contains(&ProblemCode::PendingWithoutIndexer));

    let failed_with_previous = SourceFacts {
        build_state: BuildState::Failed,
        last_error: Some("disk full".into()),
        ..facts("a")
    };
    let r = report(collected(Mode::Local, vec![failed_with_previous], vec![]));
    let p = &r.problems[0];
    assert_eq!(
        (p.code, p.severity),
        (ProblemCode::SourceBuildFailed, Severity::Warning)
    );
    assert_eq!(r.health.state, HealthState::Degraded);
    assert_eq!(r.sources[0].index_state, SourceIndexState::Completed);
    let failed_alone = SourceFacts {
        build_state: BuildState::Failed,
        active_build_id: None,
        published: None,
        ..facts("a")
    };
    let r = report(collected(Mode::Local, vec![failed_alone], vec![]));
    assert_eq!(r.health.state, HealthState::Failed);
}

#[test]
fn problems_are_complete_deduplicated_and_redacted() {
    let leaky = SourceFacts {
        access: Access::Offline,
        last_error: Some("mount https://admin:hunter2@nas.example/share failed".into()),
        ..facts("a")
    };
    let mut c = collected(
        Mode::Local,
        vec![leaky],
        vec![
            run("a", "processing", Liveness::Stalled),
            run("a", "processing", Liveness::Stalled),
        ],
    );
    c.last_run = Some(LastRun {
        run_id: None,
        operation: Some("index".into()),
        outcome: "crashed".into(),
        started_at: None,
        completed_at: None,
        error: None,
    });
    let r = report(c);
    for p in &r.problems {
        assert!(!p.message.contains("hunter2"), "{}", p.message);
        assert!(p.observed_at.is_some());
    }
    let stalled = r
        .problems
        .iter()
        .filter(|p| p.code == ProblemCode::RunStalled)
        .count();
    assert_eq!(stalled, 1, "duplicates are reported once");
    let s = r
        .problems
        .iter()
        .find(|p| p.code == ProblemCode::RunStalled)
        .unwrap();
    assert_eq!(
        (s.source_id.as_deref(), s.host.as_deref()),
        (Some("a"), Some("host-a"))
    );
    assert!(s.message.contains("600s"));
    assert!(codes(&r).contains(&ProblemCode::RunCrashed));
    assert!(codes(&r).contains(&ProblemCode::AllSourcesOffline));
    assert!(r
        .problems
        .windows(2)
        .all(|w| w[0].severity >= w[1].severity));
    assert!(r.sources[0]
        .last_error
        .as_deref()
        .is_some_and(|e| !e.contains("hunter2")));
}
