//! Cross-source dependencies: a consumer's unresolved references are bound
//! in the producers it depends on (discovered C# project references or
//! declared dependencies), only when a candidate it looks up changes, never
//! in unrelated sources, and runs that index only the producer still update
//! the consumer against its existing published base.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use docmonk_config::DocMonkConfig;
use docmonk_core::paths::{project_id_for_canonical, Home};
use docmonk_service::dependencies::DependencyOutcome;
use docmonk_service::indexing::{index_sources, selected_sources, SourceEvent};
use docmonk_storage::control::SourceRecord;
use docmonk_storage::knowledge::ProjectStore;
use docmonk_storage::StorageLayout;

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

const PRODUCER: &str = "namespace Lib {\n  public class Shared {\n    public static int Compute() { return 1; }\n  }\n}\n";

struct Env {
    _d: tempfile::TempDir,
    home: Home,
    /// consumer `a`, producer `b`, unrelated `c` (defines the same symbol).
    roots: BTreeMap<&'static str, PathBuf>,
}

fn setup(declared: &[&str]) -> Env {
    let d = tempfile::tempdir().unwrap();
    let home = Home::new(d.path().join("home"));
    home.ensure_layout().unwrap();
    let mut cfg = DocMonkConfig::default();
    cfg.updates.enabled = false;
    docmonk_config::write_user_config(&cfg, &home).unwrap();
    let mut roots = BTreeMap::new();
    let a = d.path().join("a");
    write(
        &a,
        "App/Program.cs",
        "namespace App {\n  public class Program {\n    public static int Main() { return Lib.Shared.Compute(); }\n  }\n}\n",
    );
    write(
        &a,
        "App/App.csproj",
        "<Project><ItemGroup>\n  <ProjectReference Include=\"..\\..\\b\\Lib\\Lib.csproj\" />\n  <PackageReference Include=\"Newtonsoft.Json\" Version=\"13.0.1\" />\n</ItemGroup></Project>\n",
    );
    let b = d.path().join("b");
    write(&b, "Lib/Shared.cs", PRODUCER);
    write(&b, "Lib/Lib.csproj", "<Project></Project>\n");
    let c = d.path().join("c");
    write(&c, "Lib/Shared.cs", PRODUCER);
    let mut cat = docmonk_service::sources::catalog(&home).unwrap();
    for (k, r) in [("a", a), ("b", b), ("c", c)] {
        cat.add(r.to_str().unwrap(), vec![], vec![]).unwrap();
        roots.insert(k, r);
    }
    let env = Env { _d: d, home, roots };
    if !declared.is_empty() {
        let mut cfg = docmonk_service::load(&env.home).unwrap();
        cfg.indexing.relationship_dependencies = declared
            .iter()
            .map(|d| {
                let (c, p) = d.split_once("->").unwrap();
                format!("{} -> {}", env.path(c.trim()), env.path(p.trim()))
            })
            .collect();
        docmonk_config::write_user_config(&cfg, &env.home).unwrap();
    }
    env
}

impl Env {
    fn path(&self, k: &str) -> String {
        self.roots[k].to_str().unwrap().to_owned()
    }

    fn source(&self, k: &str) -> SourceRecord {
        let want = docmonk_core::paths::resolve(&self.roots[k]).unwrap();
        docmonk_service::sources::catalog(&self.home)
            .unwrap()
            .list(false)
            .unwrap()
            .into_iter()
            .find(|s| Path::new(&s.path) == want)
            .unwrap()
    }

    fn key(&self, id: &str) -> &'static str {
        self.roots
            .keys()
            .copied()
            .find(|k| self.source(k).id == id)
            .unwrap()
    }

    /// Runs over `only` (every enabled source when empty): the dependency
    /// outcome of each consumer, by fixture key.
    fn run(&self, only: &[&str]) -> BTreeMap<&'static str, DependencyOutcome> {
        let sources = if only.is_empty() {
            selected_sources(&self.home, None).unwrap()
        } else {
            only.iter().map(|k| self.source(k)).collect()
        };
        let mut out = BTreeMap::new();
        index_sources(&self.home, &sources, "index", |e| {
            if let SourceEvent::Dependencies { source, outcome } = e {
                assert!(
                    out.insert(self.key(&source.id), outcome.clone()).is_none(),
                    "a consumer is visited once per run"
                );
            }
        })
        .unwrap();
        out
    }

    /// Runs over `only` and returns the whole run summary.
    fn summary(&self, only: &[&str]) -> docmonk_service::indexing::RunSummary {
        let sources: Vec<SourceRecord> = only.iter().map(|k| self.source(k)).collect();
        index_sources(&self.home, &sources, "index", |_| {}).unwrap()
    }

    fn active(&self, k: &str) -> String {
        self.store(k).1
    }

    fn store(&self, k: &str) -> (ProjectStore, String) {
        let s = self.source(k);
        let cp = docmonk_service::sources::control_plane(&self.home).unwrap();
        let b = cp.state(&s.id).unwrap().active_build_id.unwrap();
        let store = ProjectStore::open(
            &StorageLayout::new(&self.home),
            &project_id_for_canonical(&s.path),
            &s.id,
            8,
        )
        .unwrap();
        (store, b)
    }

    fn manifest(&self, k: &str) -> docmonk_storage::graph::ExternalManifest {
        self.store(k).0.external_manifest().unwrap().unwrap()
    }

    /// The entity `Lib.Shared.Compute` of source `k`, if defined.
    fn compute(&self, k: &str) -> Option<String> {
        let (s, b) = self.store(k);
        s.entities_by_qualified_name(&b, "Lib.Shared.Compute")
            .unwrap()
            .first()
            .map(|e| e.id.clone())
    }
}

fn updated(o: &DependencyOutcome) -> bool {
    matches!(o, DependencyOutcome::Updated { .. })
}

#[test]
fn a_project_reference_scopes_resolution_to_the_producer_only() {
    let env = setup(&[]);
    let outcomes = env.run(&[]);
    assert_eq!(outcomes.keys().copied().collect::<Vec<_>>(), ["a"]);
    assert!(updated(&outcomes["a"]), "{outcomes:?}");
    let m = env.manifest("a");
    assert_eq!(m.state, "current");
    assert_eq!(m.producers.len(), 1);
    assert_eq!(m.producers[&env.source("b").id].via, "project_reference");
    let binding = m
        .bindings
        .iter()
        .find(|b| b.reference_text == "Lib.Shared.Compute")
        .expect("bound");
    // The identical symbol in the unrelated source `c` never competes.
    assert_eq!(
        binding.producer.as_deref(),
        Some(env.source("b").id.as_str())
    );
    assert_eq!(binding.target_entity_id, env.compute("b"));
    assert_ne!(binding.target_entity_id, env.compute("c"));
    assert!(!binding.ambiguous);
    assert!(
        m.unsupported.iter().any(|u| u.contains("Newtonsoft.Json")),
        "{:?}",
        m.unsupported
    );
    // The source-local graph row is untouched.
    let (s, b) = env.store("a");
    assert!(s
        .cross_file_references(&b)
        .unwrap()
        .iter()
        .any(
            |r| r.reference_text.as_deref() == Some("Lib.Shared.Compute")
                && r.resolver == "unresolved"
        ));
    drop(s);

    // Nothing changed: the consumer is current, no work.
    let outcomes = env.run(&[]);
    assert_eq!(outcomes["a"], DependencyOutcome::Current);
}

#[test]
fn producer_edits_invalidate_consumers_only_when_a_referenced_candidate_changes() {
    let env = setup(&[]);
    env.run(&[]);
    let (a, a_build) = env.store("a");
    let a_generation = a.base_generation(&a_build).unwrap();
    drop(a);

    // Internal (body) edit in the producer, indexing only the producer.
    write(
        &env.roots["b"],
        "Lib/Shared.cs",
        &PRODUCER.replace("return 1;", "var x = 41; return x + 1;"),
    );
    let outcomes = env.run(&["b"]);
    assert_eq!(outcomes["a"], DependencyOutcome::Current);

    // The referenced definition is removed: the consumer is updated against
    // its existing base (never reindexed), and the binding disappears.
    write(
        &env.roots["b"],
        "Lib/Shared.cs",
        &PRODUCER.replace("Compute()", "Renamed()"),
    );
    let outcomes = env.run(&["b"]);
    match &outcomes["a"] {
        DependencyOutcome::Updated {
            full,
            keys_changed,
            triggered_by,
            ..
        } => {
            assert!(!full);
            assert_eq!(*keys_changed, 1);
            assert_eq!(triggered_by, &vec![env.source("b").id]);
        }
        o => panic!("{o:?}"),
    }
    assert!(env.manifest("a").bindings.is_empty());
    let (a, b) = env.store("a");
    assert_eq!(b, a_build);
    assert_eq!(
        a.base_generation(&b).unwrap(),
        a_generation,
        "consumer not reindexed"
    );
    drop(a);

    // Introducing the definition again resolves the unresolved reference.
    write(&env.roots["b"], "Lib/Shared.cs", PRODUCER);
    let outcomes = env.run(&["b"]);
    assert!(updated(&outcomes["a"]), "{outcomes:?}");
    let m = env.manifest("a");
    assert_eq!(m.bindings.len(), 1);
    assert_eq!(m.bindings[0].target_entity_id, env.compute("b"));
}

#[test]
fn declared_dependencies_detect_ambiguity_and_cycles_terminate() {
    // a depends on b (discovered) and on c (declared): two candidates.
    // b and a also depend on each other (a declared cycle).
    let env = setup(&["a -> c", "b -> a"]);
    let outcomes = env.run(&[]);
    assert_eq!(outcomes.keys().copied().collect::<Vec<_>>(), ["a", "b"]);
    let m = env.manifest("a");
    assert_eq!(m.producers.len(), 2);
    let binding = &m.bindings[0];
    assert!(binding.ambiguous);
    assert_eq!(binding.target_entity_id, None, "never guessed");
    assert!(updated(&outcomes["b"]));
    // A second pass over the cycle is stable.
    let outcomes = env.run(&[]);
    assert!(
        outcomes.values().all(|o| *o == DependencyOutcome::Current),
        "{outcomes:?}"
    );
}

#[test]
fn unavailable_consumers_and_producers_are_reported_not_current() {
    let env = setup(&[]);
    env.run(&[]);
    // A disabled producer: the consumer is stale until it is back.
    let mut cat = docmonk_service::sources::catalog(&env.home).unwrap();
    let b = env.source("b");
    cat.set_enabled(&b.id, false).unwrap();
    let outcomes = env.run(&["a"]);
    assert_eq!(outcomes["a"].state(), "stale", "{outcomes:?}");
    assert_eq!(env.manifest("a").state, "stale");
    cat.set_enabled(&b.id, true).unwrap();
    let outcomes = env.run(&["a"]);
    assert!(updated(&outcomes["a"]), "retried once the producer is back");

    // A disabled consumer impacted by a producer change is pending.
    let a = env.source("a");
    cat.set_enabled(&a.id, false).unwrap();
    write(
        &env.roots["b"],
        "Lib/Shared.cs",
        &PRODUCER.replace("Compute()", "Renamed()"),
    );
    let outcomes = env.run(&["b"]);
    assert_eq!(outcomes["a"].state(), "pending");
}

// ---- Audit remediation PH01 (F02, F03, F05, F12) ----

/// Forces the next run to recompute `k`'s bindings from scratch.
fn invalidate(env: &Env, k: &str) {
    let (mut s, _) = env.store(k);
    let mut m = s.external_manifest().unwrap().unwrap();
    m.state = "stale".into();
    s.put_external_manifest(&m).unwrap();
}

#[test]
fn f02_removing_the_last_dependency_clears_bindings() {
    let env = setup(&[]);
    env.run(&[]);
    assert_eq!(env.manifest("a").bindings.len(), 1);
    // The consumer drops its only project reference.
    write(
        &env.roots["a"],
        "App/App.csproj",
        "<Project><ItemGroup></ItemGroup></Project>\n",
    );
    let outcomes = env.run(&["a"]);
    assert!(updated(&outcomes["a"]), "{outcomes:?}");
    let m = env.manifest("a");
    assert_eq!(m.state, "current");
    assert!(m.producers.is_empty(), "{:?}", m.producers);
    assert!(m.lookups.is_empty());
    assert!(m.bindings.is_empty(), "{:?}", m.bindings);
    // An empty manifest is not reconciled again.
    let outcomes = env.run(&[]);
    assert!(!outcomes.contains_key("a"), "{outcomes:?}");
}

#[test]
fn f02_removing_one_of_two_producers_rebinds_to_the_remaining_one() {
    let env = setup(&["a -> c"]);
    env.run(&[]);
    assert!(env.manifest("a").bindings[0].ambiguous);
    let mut cfg = docmonk_service::load(&env.home).unwrap();
    cfg.indexing.relationship_dependencies.clear();
    docmonk_config::write_user_config(&cfg, &env.home).unwrap();
    let outcomes = env.run(&["a"]);
    assert!(updated(&outcomes["a"]), "{outcomes:?}");
    let m = env.manifest("a");
    assert_eq!(m.producers.len(), 1);
    assert!(!m.bindings[0].ambiguous);
    assert_eq!(m.bindings[0].target_entity_id, env.compute("b"));
}

#[test]
fn f02_incomplete_discovery_keeps_the_last_bindings_stale() {
    let env = setup(&[]);
    env.run(&[]);
    // The indexed project file can no longer be read: its references are
    // unknown, not removed.
    std::fs::remove_file(env.roots["a"].join("App/App.csproj")).unwrap();
    std::fs::create_dir(env.roots["a"].join("App/App.csproj")).unwrap();
    let outcomes = env.run(&["b"]);
    assert_eq!(outcomes["a"].state(), "stale", "{outcomes:?}");
    let m = env.manifest("a");
    assert_eq!(m.state, "stale");
    assert!(m.reason.unwrap().contains("discovery incomplete"));
    assert_eq!(m.bindings.len(), 1, "last bindings kept");
}

#[test]
fn f03_a_failed_producer_lookup_is_never_read_as_no_candidate() {
    let env = setup(&[]);
    env.run(&[]);
    let before = env.manifest("a");
    assert_eq!(before.bindings.len(), 1);
    invalidate(&env, "a");
    // Every qualified lookup in the producer now fails in SQLite.
    let (b, _) = env.store("b");
    b.connection()
        .execute_batch("DROP INDEX idx_entities_qname")
        .unwrap();
    drop(b);
    let outcomes = env.run(&["a"]);
    match &outcomes["a"] {
        DependencyOutcome::Stale { reason, .. } => {
            assert!(reason.contains("lookup in producer"), "{reason}")
        }
        o => panic!("a failed lookup was reported {o:?}"),
    }
    let m = env.manifest("a");
    assert_eq!(m.state, "stale");
    assert_eq!(m.bindings, before.bindings, "last bindings kept");
}

#[test]
fn f05_an_active_build_switch_during_the_update_fences_publication() {
    use docmonk_service::dependencies::test_hooks;
    let env = setup(&[]);
    env.run(&[]);
    invalidate(&env, "a");
    let a = env.source("a").id;
    let b = env.source("b").id;
    let home = env.home.clone();
    // The producer publishes another build (the old one stays stored)
    // between the lookups and the fence.
    test_hooks::set_before_fence(Some(Box::new(move |consumer| {
        if consumer == a {
            docmonk_service::sources::control_plane(&home)
                .unwrap()
                .connection()
                .execute(
                    "UPDATE source_state SET active_build_id = 'republished' WHERE source_id = ?1",
                    [&b],
                )
                .unwrap();
        }
    })));
    let outcomes = env.run(&["a"]);
    test_hooks::set_before_fence(None);
    match &outcomes["a"] {
        DependencyOutcome::Stale { reason, .. } => {
            assert!(reason.contains("republished"), "{reason}")
        }
        o => panic!("a fenced update was reported {o:?}"),
    }
    assert_eq!(env.manifest("a").state, "stale");
}

#[test]
fn f12_every_manifest_advance_is_a_persisted_revision() {
    let env = setup(&[]);
    env.run(&[]);
    let first = env.manifest("a");
    assert!(first.revision > 0);
    assert!(first.updated_at.is_some());
    // Implementation-only producer edit: the fast path advances the
    // producer snapshot through the same persistence path.
    write(
        &env.roots["b"],
        "Lib/Shared.cs",
        &PRODUCER.replace("return 1;", "return 2;"),
    );
    let outcomes = env.run(&["b"]);
    assert_eq!(outcomes["a"], DependencyOutcome::Current);
    let next = env.manifest("a");
    assert!(
        next.revision > first.revision,
        "{} > {}",
        next.revision,
        first.revision
    );
    let b = env.source("b").id;
    assert_ne!(
        next.producers[&b].base_generation,
        first.producers[&b].base_generation
    );
}

// ---- Audit remediation PH03 (F01) ----

fn edges(
    env: &Env,
    only: Option<&str>,
    name: &str,
    dir: docmonk_retrieval::graph::Direction,
) -> Vec<serde_json::Value> {
    let only = only.map(|k| env.source(k).id);
    let opened = docmonk_service::query::open_sources(&env.home, only.as_deref()).unwrap();
    let v = docmonk_service::query::calls_value(&opened, name, dir, 3, 100).unwrap();
    v["edges"].as_array().unwrap().clone()
}

fn external(edges: &[serde_json::Value]) -> Vec<&serde_json::Value> {
    edges.iter().filter(|e| !e["external"].is_null()).collect()
}

#[test]
fn f01_bound_references_are_traversed_across_sources() {
    use docmonk_retrieval::graph::Direction;
    let env = setup(&[]);
    env.run(&[]);
    let b = env.source("b").id;
    let a = env.source("a").id;

    // Callers of the producer's definition include the consumer's call.
    let callers = edges(&env, None, "Lib.Shared.Compute", Direction::Incoming);
    let ext = external(&callers);
    assert_eq!(ext.len(), 1, "{callers:#?}");
    assert_eq!(ext[0]["external"]["producer_source_id"], b.as_str());
    assert_eq!(ext[0]["external"]["consumer_source_id"], a.as_str());
    assert_eq!(
        ext[0]["external"]["target_entity_id"].as_str(),
        env.compute("b").as_deref()
    );

    // Callees of the consumer's entry point reach into the producer.
    let callees = edges(&env, None, "App.Program.Main", Direction::Outgoing);
    let ext = external(&callees);
    assert_eq!(ext.len(), 1, "{callees:#?}");
    assert_eq!(ext[0]["external"]["producer_source_id"], b.as_str());

    // A source filter stays a hard restriction: no hop into `b`.
    let only_a = edges(&env, Some("a"), "App.Program.Main", Direction::Outgoing);
    assert!(external(&only_a).is_empty(), "{only_a:#?}");
}

#[test]
fn f01_stale_or_ambiguous_bindings_are_never_traversed() {
    use docmonk_retrieval::graph::Direction;
    // Ambiguous (two producers define it): never traversed.
    let env = setup(&["a -> c"]);
    env.run(&[]);
    let callees = edges(&env, None, "App.Program.Main", Direction::Outgoing);
    assert!(external(&callees).is_empty(), "{callees:#?}");

    // A producer snapshot that no longer matches the producer's published
    // build is not served (read-time revalidation, F05).
    let env = setup(&[]);
    env.run(&[]);
    {
        let (mut s, _) = env.store("a");
        let mut m = s.external_manifest().unwrap().unwrap();
        for p in m.producers.values_mut() {
            p.base_generation = "moved".into();
        }
        s.put_external_manifest(&m).unwrap();
    }
    let callees = edges(&env, None, "App.Program.Main", Direction::Outgoing);
    assert!(external(&callees).is_empty(), "{callees:#?}");

    // A manifest that is no longer current is not served.
    let env = setup(&[]);
    env.run(&[]);
    invalidate(&env, "a");
    let callees = edges(&env, None, "App.Program.Main", Direction::Outgoing);
    assert!(external(&callees).is_empty(), "{callees:#?}");
}

// ---- Audit remediation PH06 (F08) ----

fn stage(
    summary: &docmonk_service::indexing::RunSummary,
    name: &str,
) -> docmonk_service::indexing::StageReport {
    summary
        .stages()
        .into_iter()
        .find(|s| s.stage == name)
        .unwrap()
}

/// Makes every scheduled dependency retry due now.
fn make_retries_due(env: &Env) {
    use docmonk_storage::dependency_retry::RetryBook;
    let mut book = RetryBook::load(&env.home);
    for e in book.consumers.values_mut() {
        e.next_attempt_at = "2000-01-01T00:00:00Z".into();
    }
    book.save(&env.home).unwrap();
}

#[test]
fn f08_a_stale_dependency_is_never_reported_as_success() {
    let env = setup(&[]);
    assert_eq!(env.summary(&["a", "b", "c"]).outcome(), "success");
    // The producer is disabled: the consumer's index and graph are fine,
    // its dependency is not.
    let mut cat = docmonk_service::sources::catalog(&env.home).unwrap();
    cat.set_enabled(&env.source("b").id, false).unwrap();
    let summary = env.summary(&["a"]);
    assert_eq!(summary.outcome(), "partial_success", "{summary:?}");
    assert_eq!(stage(&summary, "indexed").state, "current");
    assert_eq!(stage(&summary, "source_graphs").state, "current");
    let deps = stage(&summary, "cross_source_dependencies");
    assert_eq!(deps.state, "not_current");
    assert_eq!(deps.issues.len(), 1);
    assert_eq!(deps.issues[0].source_id, env.source("a").id);
    assert_eq!(deps.issues[0].code, "producer_disabled");
    assert!(!deps.issues[0].at.is_empty());
    let err = summary.into_result().unwrap_err();
    assert_eq!(
        err.kind(),
        docmonk_core::errors::ErrorKind::IndexingPartialFailure
    );
    assert_eq!(err.class(), Some("DerivedStagesIncomplete"));
    assert!(
        err.message().contains("cross-source dependencies"),
        "{err:?}"
    );
}

#[test]
fn f08_a_failed_producer_lookup_makes_the_run_partial() {
    let env = setup(&[]);
    env.run(&[]);
    invalidate(&env, "a");
    let (b, _) = env.store("b");
    b.connection()
        .execute_batch("DROP INDEX idx_entities_qname")
        .unwrap();
    drop(b);
    let summary = env.summary(&["a"]);
    assert_eq!(summary.outcome(), "partial_success");
    let deps = stage(&summary, "cross_source_dependencies");
    assert_eq!(deps.issues[0].code, "producer_lookup_failed");
}

#[test]
fn f08_a_disabled_consumer_is_not_a_required_dependency() {
    let env = setup(&[]);
    env.run(&[]);
    let mut cat = docmonk_service::sources::catalog(&env.home).unwrap();
    cat.set_enabled(&env.source("a").id, false).unwrap();
    write(
        &env.roots["b"],
        "Lib/Shared.cs",
        &PRODUCER.replace("Compute()", "Renamed()"),
    );
    let summary = env.summary(&["b"]);
    assert_eq!(summary.dependencies.pending, 1);
    assert!(summary.dependencies.unresolved.is_empty());
    assert_eq!(summary.outcome(), "success");
    assert!(summary.into_result().is_ok());
}

#[test]
fn f08_status_separates_index_ready_from_complete_graph_ready() {
    let env = setup(&[]);
    env.run(&[]);
    let report = docmonk_service::status::report(&env.home).unwrap();
    let a = report.source(&env.source("a").id).unwrap();
    assert!(a.readiness.index_ready && a.readiness.complete_graph_ready);
    assert_eq!(a.dependency_state.as_ref().unwrap().state, "current");

    let mut cat = docmonk_service::sources::catalog(&env.home).unwrap();
    cat.set_enabled(&env.source("b").id, false).unwrap();
    env.summary(&["a"]);
    let report = docmonk_service::status::report(&env.home).unwrap();
    let a = report.source(&env.source("a").id).unwrap();
    assert!(a.readiness.index_ready, "the index stays searchable");
    assert!(!a.readiness.complete_graph_ready);
    let d = a.dependency_state.as_ref().unwrap();
    assert_eq!(d.state, "stale");
    let retry = d.retry.as_ref().expect("the retry is visible in status");
    assert_eq!(retry.code, "producer_disabled");
    assert_eq!(retry.attempts, 1);
    assert!(retry.next_attempt_at.is_some() && retry.last_attempt_at.is_some());
    assert!(report.problems.iter().any(|p| {
        p.code == docmonk_status::model::ProblemCode::DependenciesNotCurrent
            && p.source_id.as_deref() == Some(a.source_id.as_str())
    }));
    let json = serde_json::to_value(&report).unwrap();
    let row = json["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["source_id"] == a.source_id.as_str())
        .unwrap();
    assert_eq!(row["dependency_state"]["state"], "stale");
    assert_eq!(row["readiness"]["complete_graph_ready"], false);
}

#[test]
fn f08_targeted_retries_converge_without_reindexing() {
    use docmonk_storage::dependency_retry::RetryBook;
    let env = setup(&[]);
    env.run(&[]);
    let a = env.source("a");
    let built = env.active("a");
    let cfg = docmonk_service::load(&env.home).unwrap();
    let backend = docmonk_service::backend::open_for_write(&env.home).unwrap();
    let timeout = std::time::Duration::from_millis(200);
    let retry = |env: &Env| {
        docmonk_service::dependencies::retry_due(&env.home, &cfg, &backend, timeout, |_, _| {})
    };

    let mut cat = docmonk_service::sources::catalog(&env.home).unwrap();
    cat.set_enabled(&env.source("b").id, false).unwrap();
    env.summary(&["a"]);
    // Scheduled with backoff, remembered on disk (a restart reloads it).
    let book = RetryBook::load(&env.home);
    assert_eq!(book.consumers[&a.id].attempts, 1);
    assert!(retry(&env).is_none(), "nothing is due before the backoff");

    // Lock contention: the retry fails again and backs off further.
    make_retries_due(&env);
    let lock = docmonk_indexing::lock::RunLock::acquire(
        &docmonk_service::indexing::source_lock_path(&env.home, &a.id),
        "test",
        Some(&a.id),
        timeout,
    )
    .unwrap();
    let s = retry(&env).unwrap();
    assert_eq!(s.unresolved[0].code, "consumer_locked");
    lock.release();
    let e = &RetryBook::load(&env.home).consumers[&a.id];
    assert_eq!(e.attempts, 2);
    assert_eq!(e.code, "consumer_locked");

    // The producer is back: the next due retry converges.
    cat.set_enabled(&env.source("b").id, true).unwrap();
    make_retries_due(&env);
    let s = retry(&env).unwrap();
    assert_eq!(s.updated, 1, "{s:?}");
    assert!(s.unresolved.is_empty());
    assert_eq!(env.manifest("a").state, "current");
    assert!(RetryBook::load(&env.home).consumers.is_empty());
    assert_eq!(env.active("a"), built, "no base reindex");
    assert!(retry(&env).is_none());

    // A warm pass with nothing changed schedules nothing.
    let summary = env.summary(&["a", "b"]);
    assert_eq!(summary.outcome(), "success");
    assert!(RetryBook::load(&env.home).consumers.is_empty());
}

#[test]
fn f08_a_manual_relationships_build_reports_unresolved_dependencies() {
    let env = setup(&[]);
    env.run(&[]);
    let mut cat = docmonk_service::sources::catalog(&env.home).unwrap();
    cat.set_enabled(&env.source("b").id, false).unwrap();
    let summary =
        docmonk_service::relationships::build(&env.home, &[env.source("a")], |_, _| {}).unwrap();
    assert_eq!(
        summary.not_current(),
        0,
        "the source graph itself is current"
    );
    assert_eq!(summary.dependencies.unresolved.len(), 1);
    assert_eq!(summary.incomplete(), 1);
}
