//! The two-stage run (post-index relationships): every selected source is
//! indexed and published before any relationship graph is built; graph
//! failures are reported separately and never retract a published index;
//! `relationships build` retries graphs without reindexing; disabling
//! relationships skips the stage and hides graphs.

use std::path::{Path, PathBuf};

use docmonk_config::DocMonkConfig;
use docmonk_core::errors::ErrorKind;
use docmonk_core::paths::{project_id_for_canonical, Home};
use docmonk_service::indexing::{index_sources, selected_sources, SourceEvent};
use docmonk_service::relationships::RelationshipOutcome;
use docmonk_storage::graph::GraphLifecycle;
use docmonk_storage::knowledge::ProjectStore;
use docmonk_storage::StorageLayout;

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

struct Env {
    _d: tempfile::TempDir,
    home: Home,
    roots: Vec<PathBuf>,
}

fn setup(relationships: bool) -> Env {
    let d = tempfile::tempdir().unwrap();
    let home = Home::new(d.path().join("home"));
    home.ensure_layout().unwrap();
    let mut cfg = DocMonkConfig::default();
    cfg.updates.enabled = false;
    cfg.indexing.relationships_enabled = relationships;
    docmonk_config::write_user_config(&cfg, &home).unwrap();
    let mut roots = Vec::new();
    let mut c = docmonk_service::sources::catalog(&home).unwrap();
    for name in ["alpha", "beta", "gamma"] {
        let r = d.path().join(name);
        write(
            &r,
            "app/main.py",
            "from app import util\n\ndef main():\n    return util.helper()\n",
        );
        write(&r, "app/util.py", "def helper():\n    return 1\n");
        write(&r, "docs/guide.md", "# Guide\n\nCall helper from main.\n");
        c.add(r.to_str().unwrap(), vec![], vec![]).unwrap();
        roots.push(r);
    }
    Env { _d: d, home, roots }
}

fn set_relationships(env: &Env, on: bool) {
    let mut cfg = docmonk_service::load(&env.home).unwrap();
    cfg.indexing.relationships_enabled = on;
    docmonk_config::write_user_config(&cfg, &env.home).unwrap();
}

/// `(source_id, phase)` in the order the run reported them.
fn run(env: &Env) -> (Vec<(String, &'static str)>, Result<(), ErrorKind>) {
    let sources = selected_sources(&env.home, None).unwrap();
    let mut events = Vec::new();
    let r = index_sources(&env.home, &sources, "index", |e| match e {
        SourceEvent::Completed { source, run } => {
            assert!(run.offline.is_none());
            events.push((source.id.clone(), "indexed"))
        }
        SourceEvent::Relationships { source, outcome } => {
            events.push((source.id.clone(), outcome.state()))
        }
        SourceEvent::Failed { source, error } | SourceEvent::Blocked { source, error } => {
            panic!("{}: {}", source.id, error.message())
        }
        SourceEvent::Started { .. } | SourceEvent::Dependencies { .. } => {}
    })
    .unwrap();
    (events, r.into_result().map(drop).map_err(|e| e.kind()))
}

fn store(env: &Env, i: usize) -> (ProjectStore, String) {
    let sources = selected_sources(&env.home, None).unwrap();
    let s = sources
        .iter()
        .find(|s| Path::new(&s.path) == docmonk_core::paths::resolve(&env.roots[i]).unwrap())
        .unwrap();
    let cp = docmonk_service::sources::control_plane(&env.home).unwrap();
    let active = cp.state(&s.id).unwrap().active_build_id.unwrap();
    let store = ProjectStore::open(
        &StorageLayout::new(&env.home),
        &project_id_for_canonical(&s.path),
        &s.id,
        8,
    )
    .unwrap();
    (store, active)
}

#[test]
fn every_source_is_indexed_before_any_relationship_graph_is_built() {
    let env = setup(true);
    let (events, r) = run(&env);
    assert_eq!(r, Ok(()));
    let first_graph = events
        .iter()
        .position(|(_, p)| *p != "indexed")
        .expect("graph stage ran");
    assert_eq!(first_graph, 3, "barrier violated: {events:?}");
    assert!(events[3..].iter().all(|(_, p)| *p == "ready"), "{events:?}");
    for i in 0..3 {
        let (s, b) = store(&env, i);
        assert!(s.graph_visible(&b).unwrap());
        assert!(s.count("relationships", &b).unwrap() > 0);
    }

    // A warm pass reindexes nothing and finds every graph current.
    let (events, r) = run(&env);
    assert_eq!(r, Ok(()));
    assert!(
        events[3..].iter().all(|(_, p)| *p == "up_to_date"),
        "{events:?}"
    );
}

#[test]
fn a_graph_failure_is_a_partial_success_that_keeps_the_index_and_can_be_retried() {
    let env = setup(true);
    let sources = selected_sources(&env.home, None).unwrap();
    let edited = docmonk_core::paths::resolve(&env.roots[0]).unwrap();
    let mut outcomes = Vec::new();
    // Edit one source's file after its index was published but before the
    // graph stage (the barrier guarantees that order).
    let summary = index_sources(&env.home, &sources, "index", |e| match e {
        SourceEvent::Completed { source, .. } if Path::new(&source.path) == edited => write(
            &env.roots[0],
            "app/util.py",
            "def helper():\n    return 2\n",
        ),
        SourceEvent::Relationships { source, outcome } => {
            outcomes.push((source.path.clone(), outcome.state()));
            if Path::new(&source.path) == edited {
                assert!(
                    matches!(outcome, RelationshipOutcome::Stale(_)),
                    "{outcome:?}"
                );
            }
        }
        _ => {}
    })
    .unwrap();
    assert_eq!(summary.failed_sources, 0);
    assert_eq!(summary.failed_files, 0);
    assert_eq!(summary.relationships.stale, 1);
    assert_eq!(summary.relationships.built, 2);
    assert_eq!(summary.outcome(), "partial_success");
    let err = summary.into_result().unwrap_err();
    assert_eq!(err.kind(), ErrorKind::IndexingPartialFailure);
    assert!(
        err.message().contains("relationships were not published"),
        "{}",
        err.message()
    );

    // The index of the edited source is published and searchable; its
    // graph is not visible and nothing of it was written.
    let (s, b) = store(&env, 0);
    assert!(s.count("entities", &b).unwrap() > 0);
    assert!(!s.search_code(&b, "helper", 5).unwrap().is_empty());
    assert!(!s.graph_visible(&b).unwrap());
    assert_eq!(s.count("relationships", &b).unwrap(), 0);
    assert_eq!(
        s.graph_status(Some(&b)).unwrap().state,
        GraphLifecycle::Stale
    );
    drop(s);

    // A graph-only retry still refuses the mismatched snapshot...
    let only = vec![sources
        .iter()
        .find(|s| Path::new(&s.path) == edited)
        .unwrap()
        .clone()];
    let r = docmonk_service::relationships::build(&env.home, &only, |_, _| {}).unwrap();
    assert_eq!((r.stale, r.built), (1, 0));
    // ...and succeeds, without reindexing, once the index caught up.
    let (_, r) = run(&env);
    assert_eq!(r, Ok(()));
    let r = docmonk_service::relationships::build(&env.home, &only, |_, _| {}).unwrap();
    assert_eq!((r.up_to_date, r.not_current()), (1, 0));
    let (s, b) = store(&env, 0);
    assert!(s.graph_visible(&b).unwrap());
}

#[test]
fn disabled_relationships_skip_the_stage_and_hide_graphs_without_reindexing() {
    let env = setup(true);
    assert_eq!(run(&env).1, Ok(()));
    let (s, b) = store(&env, 1);
    let parser = s.files(&b).unwrap()[0].parser_version.clone();
    drop(s);

    set_relationships(&env, false);
    let (events, r) = run(&env);
    assert_eq!(r, Ok(()));
    assert!(events.iter().all(|(_, p)| *p == "indexed"), "{events:?}");
    let (s, b2) = store(&env, 1);
    assert_eq!(b2, b, "toggling relationships never forces a rebuild");
    assert_eq!(s.files(&b2).unwrap()[0].parser_version, parser);
    assert!(!s.graph_visible(&b2).unwrap());
    assert_eq!(
        s.graph_status(Some(&b2)).unwrap().state,
        GraphLifecycle::Disabled
    );
    // Graph readers see nothing; regular search still works.
    assert!(s.entity_links(&b2, "x").unwrap().is_empty());
    assert!(!s.search_code(&b2, "helper", 5).unwrap().is_empty());
    drop(s);
    assert!(docmonk_service::relationships::build(
        &env.home,
        &selected_sources(&env.home, None).unwrap(),
        |_, _| {}
    )
    .is_err());

    // Re-enabled: the retained graph is re-validated, not reindexed.
    set_relationships(&env, true);
    let (events, r) = run(&env);
    assert_eq!(r, Ok(()));
    assert!(events[3..].iter().all(|(_, p)| *p == "ready"), "{events:?}");
    let (s, b3) = store(&env, 1);
    assert_eq!(b3, b);
    assert!(s.graph_visible(&b3).unwrap());
}

#[test]
fn a_disabled_fresh_home_never_builds_a_graph() {
    let env = setup(false);
    let (events, r) = run(&env);
    assert_eq!(r, Ok(()));
    assert_eq!(events.len(), 3, "{events:?}");
    for i in 0..3 {
        let (s, b) = store(&env, i);
        assert_eq!(s.count("relationships", &b).unwrap(), 0);
        assert_eq!(s.count("cross_links", &b).unwrap(), 0);
        assert!(s.count("entities", &b).unwrap() > 0);
    }
}

/// The graph of `b` as normalized semantic tuples (ids are content-addressed,
/// so equal tuples mean equal graphs).
type Graph = (Vec<String>, Vec<String>);
fn graph_of(s: &ProjectStore, b: &str) -> Graph {
    let mut edges = Vec::new();
    for f in s.files(b).unwrap() {
        for r in s.file_relationships(b, &f.id).unwrap() {
            edges.push(format!(
                "{}|{}|{:?}|{:?}|{}|{}",
                r.id, r.file_id, r.target_entity_id, r.target_symbol, r.resolver, r.confidence
            ));
        }
    }
    let mut links: Vec<String> = s
        .links(b)
        .unwrap()
        .into_iter()
        .map(|l| {
            format!(
                "{}|{}|{}|{:?}|{}",
                l.id, l.entity_id, l.document_id, l.chunk_id, l.resolver
            )
        })
        .collect();
    edges.sort();
    links.sort();
    (edges, links)
}

fn graph_runs(
    env: &Env,
    run: &docmonk_service::indexing::RunOptions,
) -> Vec<docmonk_code::graph_stage::GraphReport> {
    let sources = selected_sources(&env.home, None).unwrap();
    let mut out = Vec::new();
    docmonk_service::indexing::index_sources_with(&env.home, &sources, "index", run, |e| {
        if let SourceEvent::Relationships { outcome, .. } = e {
            out.push(outcome.run().expect("graph published").graph.clone());
        }
    })
    .unwrap();
    out
}

#[test]
fn a_full_rebuild_with_unchanged_content_reuses_the_graph_without_deriving_anything() {
    let env = setup(true);
    assert_eq!(run(&env).1, Ok(()));
    let (s, b1) = store(&env, 0);
    let before = graph_of(&s, &b1);
    assert!(!before.0.is_empty() && !before.1.is_empty());
    drop(s);

    let reports = graph_runs(
        &env,
        &docmonk_service::indexing::RunOptions {
            force_full: true,
            ..Default::default()
        },
    );
    assert_eq!(reports.len(), 3);
    for r in &reports {
        assert_eq!(
            r.action,
            docmonk_code::graph_stage::PlanAction::Rebind,
            "{r:?}"
        );
        assert_eq!(
            (
                r.files_processed,
                r.files_reresolved,
                r.relationships_written
            ),
            (0, 0, 0)
        );
    }
    let (s, b2) = store(&env, 0);
    assert_ne!(b1, b2, "a new base build");
    assert!(s.graph_visible(&b2).unwrap());
    assert_eq!(graph_of(&s, &b2), before);
}

#[test]
fn a_full_rebuild_with_one_edit_derives_only_that_file_and_matches_a_clean_build() {
    let env = setup(true);
    assert_eq!(run(&env).1, Ok(()));
    write(
        &env.roots[0],
        "app/util.py",
        "def helper():\n    return 3\n\ndef extra():\n    return helper()\n",
    );
    let reports = graph_runs(
        &env,
        &docmonk_service::indexing::RunOptions {
            force_full: true,
            ..Default::default()
        },
    );
    let edited: Vec<_> = reports
        .iter()
        .filter(|r| r.action != docmonk_code::graph_stage::PlanAction::Rebind)
        .collect();
    assert_eq!(edited.len(), 1, "{reports:?}");
    assert_eq!(edited[0].files_processed, 1);
    assert!(!edited[0].full);
    let (s, b) = store(&env, 0);
    let incremental = graph_of(&s, &b);
    drop(s);

    // Oracle: the same snapshot derived from scratch.
    let (mut s, b) = store(&env, 0);
    s.clear_graph(&b).unwrap();
    s.put_graph_state_with_files(&Default::default()).unwrap();
    let r =
        docmonk_knowledge::build_graph(&mut s, &env.roots[0], &b, 1, None, &mut |_, _| {}).unwrap();
    assert!(r.full);
    assert_eq!(graph_of(&s, &b), incremental);
}

#[test]
fn a_graph_adopted_from_elsewhere_is_rebound_without_parsing() {
    // What a server writer does after its staging cache was deleted: the
    // rows and manifest come from the backend, not from local state.
    let env = setup(true);
    assert_eq!(run(&env).1, Ok(()));
    let (mut s, b) = store(&env, 0);
    let before = graph_of(&s, &b);
    let state = s.graph_state_with_files().unwrap();
    let edges: Vec<_> = s
        .files(&b)
        .unwrap()
        .iter()
        .flat_map(|f| s.file_relationships(&b, &f.id).unwrap())
        .collect();
    let links = s.links(&b).unwrap();
    s.clear_graph(&b).unwrap();
    s.put_graph_state_with_files(&Default::default()).unwrap();
    s.adopt_graph(
        &b,
        &edges,
        &links,
        state.files.clone(),
        state.derivation_version.as_deref().unwrap(),
        state.input_digest.as_deref(),
    )
    .unwrap();
    assert!(
        !s.graph_visible(&b).unwrap(),
        "adopted rows are not visible before validation"
    );
    let r =
        docmonk_knowledge::build_graph(&mut s, &env.roots[0], &b, 1, None, &mut |_, _| {}).unwrap();
    assert_eq!(r.action, docmonk_code::graph_stage::PlanAction::Rebind);
    assert_eq!(r.files_processed, 0);
    assert!(s.graph_visible(&b).unwrap());
    assert_eq!(graph_of(&s, &b), before);
}

/// Rebuilds source `i`'s graph from scratch and returns it (the oracle).
fn oracle(env: &Env, i: usize) -> Graph {
    let (mut s, b) = store(env, i);
    s.clear_graph(&b).unwrap();
    s.put_graph_state_with_files(&Default::default()).unwrap();
    docmonk_knowledge::build_graph(&mut s, &env.roots[i], &b, 1, None, &mut |_, _| {}).unwrap();
    graph_of(&s, &b)
}

fn report_of(
    reports: &[docmonk_code::graph_stage::GraphReport],
) -> &docmonk_code::graph_stage::GraphReport {
    let changed: Vec<_> = reports
        .iter()
        .filter(|r| r.action != docmonk_code::graph_stage::PlanAction::Skip)
        .collect();
    assert_eq!(changed.len(), 1, "{reports:?}");
    changed[0]
}

#[test]
fn a_body_edit_does_not_re_resolve_consumers_and_a_removed_definition_does() {
    let env = setup(true);
    assert_eq!(run(&env).1, Ok(()));
    let default = docmonk_service::indexing::RunOptions::default();

    // Body-only edit of the definition main.py calls: only util.py.
    write(
        &env.roots[0],
        "app/util.py",
        "def helper():\n    x = 41\n    return x + 1\n",
    );
    let reports = graph_runs(&env, &default);
    let r = report_of(&reports);
    assert_eq!(r.action, docmonk_code::graph_stage::PlanAction::Incremental);
    assert_eq!((r.files_processed, r.files_reresolved), (1, 1), "{r:?}");
    let (s, b) = store(&env, 0);
    let incremental = graph_of(&s, &b);
    drop(s);
    assert_eq!(oracle(&env, 0), incremental);

    // Removing the definition re-resolves its consumer and cleans its links.
    write(&env.roots[0], "app/util.py", "def other():\n    return 1\n");
    let reports = graph_runs(&env, &default);
    let r = report_of(&reports);
    assert_eq!((r.files_processed, r.files_reresolved), (1, 2), "{r:?}");
    let (s, b) = store(&env, 0);
    let incremental = graph_of(&s, &b);
    drop(s);
    assert_eq!(oracle(&env, 0), incremental);

    // Adding it back resolves the unchanged consumer again.
    write(
        &env.roots[0],
        "app/util.py",
        "def helper():\n    return 1\n",
    );
    let reports = graph_runs(&env, &default);
    assert_eq!(report_of(&reports).files_reresolved, 2);
    let (s, b) = store(&env, 0);
    let incremental = graph_of(&s, &b);
    drop(s);
    assert_eq!(oracle(&env, 0), incremental);
}

#[test]
fn an_edited_manual_link_intent_is_applied_without_any_index_change() {
    let env = setup(true);
    assert_eq!(run(&env).1, Ok(()));
    let (mut s, b) = store(&env, 0);
    let entity = s
        .all_entities(&b)
        .unwrap()
        .into_iter()
        .find(|e| e.name == "helper")
        .unwrap();
    // Recorded intent only (as another writer or an interrupted command
    // would leave it): nothing is materialized yet.
    s.add_manual_link(&docmonk_storage::knowledge::ManualLink {
        id: "manual-1".into(),
        link_type: "documented_by".into(),
        entity_qualified_name: entity.qualified_name,
        document_rel_path: "docs/guide.md".into(),
        attachment_index: None,
        chunk_ordinal: None,
        note: None,
        created_at: "2026-01-01T00:00:00Z".into(),
    })
    .unwrap();
    drop(s);
    let reports = graph_runs(&env, &docmonk_service::indexing::RunOptions::default());
    let r = report_of(&reports);
    assert_eq!(r.files_processed, 0);
    let (s, b) = store(&env, 0);
    assert!(s.graph_visible(&b).unwrap());
    assert!(
        s.links(&b).unwrap().iter().any(|l| l.resolver == "user"),
        "manual link applied"
    );
    let incremental = graph_of(&s, &b);
    drop(s);
    assert_eq!(oracle(&env, 0), incremental);
    // And it is now current.
    let reports = graph_runs(&env, &docmonk_service::indexing::RunOptions::default());
    assert!(reports
        .iter()
        .all(|r| r.action == docmonk_code::graph_stage::PlanAction::Skip));
}

#[test]
fn an_unchanged_run_reports_every_graph_skipped_with_no_graph_work() {
    let env = setup(true);
    let sources = selected_sources(&env.home, None).unwrap();
    let first = index_sources(&env.home, &sources, "index", |_| {}).unwrap();
    assert_eq!(
        first.relationships.graph_full, 3,
        "first indexing initializes every graph"
    );
    let s = index_sources(&env.home, &sources, "index", |_| {}).unwrap();
    let r = &s.relationships;
    assert_eq!((r.graph_skipped, r.up_to_date), (3, 3), "{r:?}");
    assert_eq!(
        (
            r.graph_rebound + r.graph_incremental + r.graph_full + r.graph_retried,
            r.files_processed,
            r.files_reresolved,
            r.relationships_written
        ),
        (0, 0, 0, 0),
        "{r:?}"
    );
    let json = serde_json::to_value(r).unwrap();
    assert_eq!(json["graph_skipped"], 3);
}

#[test]
fn a_cancelled_graph_build_publishes_nothing_and_the_retry_matches_a_clean_build() {
    let env = setup(true);
    assert_eq!(run(&env).1, Ok(()));
    // Publish a new base without its graph.
    set_relationships(&env, false);
    write(
        &env.roots[0],
        "app/util.py",
        "def helper():\n    return 7\n\ndef more():\n    return helper()\n",
    );
    assert_eq!(run(&env).1, Ok(()));
    let (mut s, b) = store(&env, 0);
    let before = s.count("relationships", &b).unwrap();
    let cancel = std::sync::atomic::AtomicBool::new(true);
    let e =
        docmonk_knowledge::build_graph(&mut s, &env.roots[0], &b, 1, Some(&cancel), &mut |_, _| {})
            .unwrap_err();
    assert_eq!(e, docmonk_code::graph_stage::GraphError::Cancelled);
    assert!(!s.graph_visible(&b).unwrap());
    assert_eq!(s.graph_state().unwrap().state, GraphLifecycle::Failed);
    assert_eq!(s.count("relationships", &b).unwrap(), before, "rolled back");
    drop(s);

    set_relationships(&env, true);
    let reports = graph_runs(&env, &docmonk_service::indexing::RunOptions::default());
    let retried: Vec<_> = reports
        .iter()
        .filter(|r| r.action == docmonk_code::graph_stage::PlanAction::Retry)
        .collect();
    assert_eq!(retried.len(), 1, "{reports:?}");
    assert!(!retried[0].full);
    let (s, b) = store(&env, 0);
    let incremental = graph_of(&s, &b);
    drop(s);
    assert_eq!(oracle(&env, 0), incremental);
}
