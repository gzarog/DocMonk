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
        DependencyOutcome::Stale { reason } => {
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
        DependencyOutcome::Stale { reason } => assert!(reason.contains("republished"), "{reason}"),
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
