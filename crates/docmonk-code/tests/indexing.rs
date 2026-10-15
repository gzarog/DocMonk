//! Code intelligence through the indexer and the post-index graph stage:
//! parse-failure isolation, whole-build resolution matching the expected
//! relationships, incremental re-resolution (matching a full
//! recomputation), stale-snapshot protection, determinism and graph
//! traversal.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use docmonk_code::graph::{traverse_symbol, Direction};
use docmonk_code::graph_stage::{build_graph, GraphError, GraphOptions, GraphOutcome, GraphReport};
use docmonk_code::process::{entity_ids, prepare_source, PreparedCode};
use docmonk_core::ids::record;
use docmonk_core::paths::project_id_for_canonical;
use docmonk_indexing::coordinator::{run_source, NoProgress, Options};
use docmonk_storage::control::{ControlPlane, SourceRecord};
use docmonk_storage::graph::GraphLifecycle;
use docmonk_storage::knowledge::ProjectStore;
use docmonk_storage::StorageLayout;
use serde_json::{json, Value};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn copy_tree(from: &Path, to: &Path) {
    for e in std::fs::read_dir(from).unwrap() {
        let p = e.unwrap().path();
        let dest = to.join(p.file_name().unwrap());
        if p.is_dir() {
            std::fs::create_dir_all(&dest).unwrap();
            copy_tree(&p, &dest);
        } else {
            let text = std::fs::read(&p).unwrap();
            let text = String::from_utf8_lossy(&text).replace("\r\n", "\n");
            std::fs::write(dest, text).unwrap();
        }
    }
}

fn opts() -> Options {
    let mut o = Options::from_config(&docmonk_config::DocMonkConfig::default());
    o.workers = 4;
    o
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    layout: StorageLayout,
    cp: ControlPlane,
    src: SourceRecord,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("corpus");
    std::fs::create_dir_all(&root).unwrap();
    copy_tree(&repo_root().join("fixtures/code"), &root);
    let home = common::home(tmp.path());
    let layout = StorageLayout::new(&home);
    let mut cp = common::control(&home);
    let src = common::add_source(&mut cp, &root, &[], &[]);
    Fixture {
        _tmp: tmp,
        root,
        layout,
        cp,
        src,
    }
}

impl Fixture {
    /// Phase 1 only: the base index.
    fn index_base(&mut self) -> docmonk_indexing::coordinator::SourceResult {
        run_source(
            &self.layout,
            &mut self.cp,
            &self.src,
            &docmonk_code::registry(),
            &opts(),
            &mut NoProgress,
        )
        .unwrap()
    }

    /// Phase 2 only: the relationship graph of the published build.
    fn graph(&self) -> Result<GraphReport, GraphError> {
        let (mut store, active) = self.store();
        build_graph(
            &mut store,
            &self.root,
            &active,
            &GraphOptions {
                workers: 3,
                linker: None,
                cancel: None,
                phase: None,
                facts: None,
                batch: 0,
            },
            &mut |_, _| {},
        )
    }

    /// Both phases, like `docmonk index`.
    fn index(&mut self) -> docmonk_indexing::coordinator::SourceResult {
        let r = self.index_base();
        self.graph().unwrap();
        r
    }

    fn store(&self) -> (ProjectStore, String) {
        let active = self
            .cp
            .state(&self.src.id)
            .unwrap()
            .active_build_id
            .unwrap();
        let store = ProjectStore::open(
            &self.layout,
            &project_id_for_canonical(&self.src.path),
            &self.src.id,
            8,
        )
        .unwrap();
        (store, active)
    }

    fn count(&self, sql: &str) -> i64 {
        let (store, active) = self.store();
        store
            .connection()
            .query_row(sql, [&active], |r| r.get(0))
            .unwrap()
    }
}

/// Order-independent sort key: objects serialized with sorted keys (the
/// workspace may enable serde_json's `preserve_order`).
fn sort_key(v: &Value) -> String {
    fn sorted(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let b: std::collections::BTreeMap<_, _> =
                    m.iter().map(|(k, x)| (k.clone(), sorted(x))).collect();
                Value::Object(b.into_iter().collect())
            }
            Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
            x => x.clone(),
        }
    }
    serde_json::to_string(&sorted(v)).unwrap()
}

#[test]
fn cold_build_resolves_as_expected_and_isolates_parse_failures() {
    let mut f = fixture();
    let r = f.index();
    assert!(r.published, "{r:?}");
    assert_eq!(r.failed, 1, "only broken.py fails");
    let (store, active) = f.store();
    let files = store.files(&active).unwrap();
    let status: HashMap<_, _> = files
        .iter()
        .map(|x| (x.rel_path.clone(), x.status.clone()))
        .collect();
    assert_eq!(status["python/broken.py"], "failed");
    assert_eq!(status["python/animals.py"], "indexed");
    assert_eq!(status["misc/script.rb"], "indexed");

    // Translate record ids back to the expected file's "<rel>#<local>" keys.
    let mut key_of: HashMap<String, String> = HashMap::new();
    for file in &files {
        let source = std::fs::read(f.root.join(&file.rel_path)).unwrap();
        if let Ok(PreparedCode::Parsed { extraction, .. }) = prepare_source(&source, &file.rel_path)
        {
            assert_eq!(file.id, record::file_id(&f.src.id, &file.rel_path));
            for (i, id) in entity_ids(&file.id, &extraction).into_iter().enumerate() {
                key_of.insert(id, format!("{}#{i}", file.rel_path));
            }
        }
    }
    let golden: Value = serde_json::from_str(
        &std::fs::read_to_string(repo_root().join("fixtures/expected/code.json")).unwrap(),
    )
    .unwrap();
    let mut mismatches = Vec::new();
    for file in &files {
        let want = &golden["files"][&file.rel_path]["relationships"];
        let Some(want) = want.as_array() else {
            continue;
        };
        let mut want = want.clone();
        let mut got: Vec<Value> = store
            .file_relationships(&active, &file.id)
            .unwrap()
            .into_iter()
            .map(|r| {
                json!({
                    "type": r.relationship_type,
                    "source": key_of[&r.source_entity_id],
                    "target": r.target_entity_id.as_ref().map(|t| key_of[t].clone()),
                    "symbol": r.target_symbol, "resolver": r.resolver,
                    "confidence": r.confidence, "location": r.source_location,
                    "evidence": r.evidence,
                })
            })
            .collect();
        let key = |v: &Value| sort_key(v);
        got.sort_by_key(key);
        want.sort_by_key(key);
        if got != want {
            mismatches.push(format!(
                "{}:\n got      {got:?}\n expected {want:?}",
                file.rel_path
            ));
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

#[test]
fn incremental_builds_re_resolve_cross_file_references() {
    let mut f = fixture();
    f.index();
    let unresolved = "SELECT COUNT(*) FROM relationships WHERE build_id = ?1
        AND reference_text = 'unknown_function' AND target_entity_id IS NULL";
    assert_eq!(f.count(unresolved), 1);

    // A new file defines the symbol; the unchanged caller now resolves.
    common::write(
        &f.root,
        "python/pkg/late.py",
        "def unknown_function():\n    return 0\n",
    );
    let r = f.index();
    assert!(r.published);
    assert_eq!(f.count(unresolved), 0);
    let resolved = "SELECT COUNT(*) FROM relationships r JOIN entities e
        ON e.build_id = r.build_id AND e.id = r.target_entity_id
        WHERE r.build_id = ?1 AND r.reference_text = 'unknown_function'
          AND r.resolver = 'name_only'";
    assert_eq!(f.count(resolved), 1);

    // Deleting the target leaves no dangling edge anywhere.
    std::fs::remove_file(f.root.join("python/pkg/late.py")).unwrap();
    std::fs::remove_file(f.root.join("python/pkg/helpers.py")).unwrap();
    assert!(f.index().published);
    let dangling = "SELECT COUNT(*) FROM relationships r WHERE r.build_id = ?1
        AND r.target_entity_id IS NOT NULL AND NOT EXISTS (
          SELECT 1 FROM entities e WHERE e.build_id = r.build_id AND e.id = r.target_entity_id)";
    assert_eq!(f.count(dangling), 0);
    assert_eq!(f.count(unresolved), 1);
    let pending = "SELECT COUNT(*) FROM relationships WHERE build_id = ?1 AND resolver = 'pending'";
    assert_eq!(f.count(pending), 0);
}

#[test]
fn rebuilding_from_scratch_reproduces_identical_ids() {
    fn dump(f: &Fixture) -> BTreeMap<String, String> {
        let (store, active) = f.store();
        let mut out = BTreeMap::new();
        for file in store.files(&active).unwrap() {
            for r in store.file_relationships(&active, &file.id).unwrap() {
                out.insert(r.id.clone(), format!("{r:?}"));
            }
            for e in store.file_entities(&active, &file.id).unwrap() {
                out.insert(e.id.clone(), format!("{e:?}"));
            }
        }
        out
    }
    let mut f = fixture();
    f.index();
    let first = dump(&f);
    assert!(first.len() > 300, "{}", first.len());

    // Rebuild the same source from scratch in a fresh, empty home (open
    // handles keep Windows from deleting the first one).
    let home_root = f
        .layout
        .state_dir()
        .parent()
        .unwrap()
        .with_file_name("home-again");
    let home = docmonk_core::paths::Home::new(&home_root);
    f.layout = StorageLayout::new(&home);
    f.cp = common::control(&home);
    let src = common::add_source(&mut f.cp, &f.root.clone(), &[], &[]);
    assert_eq!(src.id, f.src.id);
    f.src = src;
    f.index();
    assert_eq!(dump(&f), first);
}

#[test]
fn callers_and_callees_traverse_the_build() {
    let mut f = fixture();
    f.index();
    let (store, active) = f.store();
    let (matches, edges) = traverse_symbol(
        &store,
        &active,
        "python.animals.Dog.bark",
        Direction::Incoming,
        &["calls"],
        1,
        100,
    )
    .unwrap();
    assert_eq!(matches.len(), 1);
    let evidence: Vec<_> = edges
        .iter()
        .map(|e| e.relationship.evidence.clone().unwrap())
        .collect();
    assert!(evidence.contains(&"d.bark".to_owned()), "{evidence:?}");
    // `dog.bark` in another file resolves by bare name to the first `bark`
    // by qualified name (Java's).
    assert!(!evidence.contains(&"dog.bark".to_owned()), "{evidence:?}");

    let (_, out) = traverse_symbol(
        &store,
        &active,
        "python.pkg.service.Kennel",
        Direction::Outgoing,
        &[],
        2,
        100,
    )
    .unwrap();
    assert!(
        out.iter().any(|e| e.depth == 2),
        "two hops reach method bodies"
    );
    assert!(out.windows(2).all(|w| w[0].depth <= w[1].depth));

    let (_, limited) = traverse_symbol(
        &store,
        &active,
        "python.pkg.service.Kennel",
        Direction::Outgoing,
        &[],
        3,
        2,
    )
    .unwrap();
    assert_eq!(limited.len(), 2);
}

fn relationships_dump(f: &Fixture) -> BTreeMap<String, String> {
    let (store, active) = f.store();
    let mut out = BTreeMap::new();
    for file in store.files(&active).unwrap() {
        for r in store.file_relationships(&active, &file.id).unwrap() {
            out.insert(r.id.clone(), format!("{r:?}"));
        }
    }
    out
}

#[test]
fn the_primary_index_never_derives_relationships() {
    let mut f = fixture();
    let r = f.index_base();
    assert!(r.published);
    let all = "SELECT COUNT(*) FROM relationships WHERE build_id = ?1";
    assert_eq!(f.count(all), 0, "phase 1 wrote relationships");
    assert!(f.count("SELECT COUNT(*) FROM entities WHERE build_id = ?1") > 50);
    let (store, active) = f.store();
    assert!(!store.graph_visible(&active).unwrap());
    assert_eq!(
        store.graph_status(Some(&active)).unwrap().state,
        GraphLifecycle::Pending
    );
    drop(store);

    let g = f.graph().unwrap();
    assert_eq!(g.outcome, GraphOutcome::Built);
    assert!(g.full);
    assert!(f.count(all) > 100);
    let (store, active) = f.store();
    assert!(store.graph_visible(&active).unwrap());
    drop(store);
    // A second graph pass over the same base is a no-op.
    assert_eq!(f.graph().unwrap().outcome, GraphOutcome::UpToDate);
}

#[test]
fn a_republished_base_hides_the_old_graph_until_it_is_rebuilt() {
    let mut f = fixture();
    f.index();
    common::write(
        &f.root,
        "python/pkg/late.py",
        "def unknown_function():\n    return 0\n",
    );
    assert!(f.index_base().published);
    let (store, active) = f.store();
    assert!(!store.graph_visible(&active).unwrap());
    assert_eq!(
        store.graph_status(Some(&active)).unwrap().state,
        GraphLifecycle::Stale
    );
    // Graph readers return nothing for a stale graph.
    assert!(store
        .edges(
            &active,
            docmonk_storage::knowledge::Endpoint::Source,
            "x",
            &[],
            10
        )
        .unwrap()
        .is_empty());
    drop(store);
    let g = f.graph().unwrap();
    assert!(!g.full, "an incremental update is enough");
    assert_eq!(g.files_processed, 1);
    let (store, active) = f.store();
    assert!(store.graph_visible(&active).unwrap());
}

#[test]
fn incremental_graph_updates_match_a_full_recomputation() {
    let mut f = fixture();
    f.index();
    // Additions, a change, a deletion and a move between graph builds.
    common::write(
        &f.root,
        "python/pkg/late.py",
        "def unknown_function():\n    return 0\n",
    );
    std::fs::remove_file(f.root.join("python/pkg/helpers.py")).unwrap();
    let text = std::fs::read_to_string(f.root.join("python/animals.py")).unwrap();
    std::fs::write(
        f.root.join("python/animals.py"),
        format!("{text}\ndef bark():\n    pass\n"),
    )
    .unwrap();
    std::fs::rename(
        f.root.join("python/pkg/service.py"),
        f.root.join("python/pkg/service2.py"),
    )
    .unwrap();
    f.index_base();
    let g = f.graph().unwrap();
    assert!(!g.full);
    let incremental = relationships_dump(&f);

    // Forget the dependency metadata: the next graph is recomputed fully.
    let (mut store, _) = f.store();
    store
        .put_graph_state_with_files(&docmonk_storage::graph::GraphState::default())
        .unwrap();
    drop(store);
    let g = f.graph().unwrap();
    assert!(g.full);
    assert_eq!(relationships_dump(&f), incremental);
}

#[test]
fn a_file_edited_after_indexing_never_yields_a_mixed_graph() {
    let mut f = fixture();
    f.index_base();
    // Changed on disk after the base was published, before the graph.
    let p = f.root.join("python/animals.py");
    let text = std::fs::read_to_string(&p).unwrap();
    std::fs::write(&p, format!("{text}\n# edited\n")).unwrap();
    match f.graph() {
        Err(GraphError::Stale(reason)) => assert!(reason.contains("python/animals.py"), "{reason}"),
        other => panic!("expected a stale graph, got {other:?}"),
    }
    // Nothing of the graph was published; the base index is untouched.
    assert_eq!(
        f.count("SELECT COUNT(*) FROM relationships WHERE build_id = ?1"),
        0
    );
    assert!(f.count("SELECT COUNT(*) FROM entities WHERE build_id = ?1") > 50);
    let (store, active) = f.store();
    assert_eq!(
        store.graph_status(Some(&active)).unwrap().state,
        GraphLifecycle::Stale
    );
    drop(store);
    // The next index pass picks the edit up and the graph is retried.
    f.index_base();
    assert_eq!(f.graph().unwrap().outcome, GraphOutcome::Built);
}

#[test]
fn a_graph_build_reports_its_phases_timings_and_counters() {
    let mut f = fixture();
    f.index_base();
    let (mut store, active) = f.store();
    let phases = std::cell::RefCell::new(Vec::new());
    let phase = |p: &'static str| phases.borrow_mut().push(p);
    let r = build_graph(
        &mut store,
        &f.root,
        &active,
        &GraphOptions {
            workers: 2,
            linker: None,
            cancel: None,
            phase: Some(&phase),
            facts: None,
            batch: 0,
        },
        &mut |_, _| {},
    )
    .unwrap();
    assert_eq!(
        phases.into_inner(),
        ["deriving", "resolving", "linking", "committing"]
    );
    let code = f.count(
        "SELECT COUNT(*) FROM files WHERE build_id = ?1 AND kind = 'code' AND status = 'indexed'",
    ) as usize;
    let c = &r.counters;
    assert_eq!(c.files_derived, code, "{r:?}");
    assert!(c.files_total >= code && c.bytes_read > 0, "{r:?}");
    assert!(c.resolution.references_examined > 0, "{r:?}");
    assert_eq!(c.resolution.changed, r.resolutions_changed);
    assert!(c.sql_statements > 0, "{r:?}");
    let t = &r.timings;
    let phases = t.state_load_seconds
        + t.clear_seconds
        + t.derivation_seconds
        + t.sqlite_write_seconds
        + t.symbol_manifest_seconds
        + t.resolution_seconds
        + t.linking_seconds
        + t.commit_seconds;
    assert!(
        phases > 0.0 && phases <= r.seconds,
        "{t:?} vs {}",
        r.seconds
    );

    // A skip reports its plan and does no work.
    let r = build_graph(
        &mut store,
        &f.root,
        &active,
        &GraphOptions {
            workers: 2,
            linker: None,
            cancel: None,
            phase: None,
            facts: None,
            batch: 0,
        },
        &mut |_, _| {},
    )
    .unwrap();
    assert_eq!(r.action.as_str(), "skip");
    assert_eq!(r.counters.files_derived, 0);
}

#[test]
fn graph_report_json_keeps_its_fields_and_adds_optional_ones() {
    let v = serde_json::to_value(GraphReport::default()).unwrap();
    for key in [
        "outcome",
        "action",
        "reason",
        "full",
        "generation",
        "base_generation",
        "files_processed",
        "relationships_written",
        "resolutions_changed",
        "files_reresolved",
        "links",
        "files_added",
        "files_modified",
        "files_removed",
        "planning_seconds",
        "seconds",
    ] {
        assert!(v.get(key).is_some(), "{key} missing from {v}");
    }
    assert_eq!(v["action"], "skip");
    assert_eq!(v["outcome"], "up_to_date");
    assert!(v["timings"]["resolution_seconds"].is_number());
    assert!(v["counters"]["sql_statements"].is_number());
    assert!(v["linking"]["units_loaded"].is_number());
}

#[test]
fn a_cancelled_graph_build_publishes_nothing() {
    let mut f = fixture();
    f.index_base();
    let (mut store, active) = f.store();
    let cancel = std::sync::atomic::AtomicBool::new(true);
    let r = build_graph(
        &mut store,
        &f.root,
        &active,
        &GraphOptions {
            workers: 2,
            linker: None,
            cancel: Some(&cancel),
            phase: None,
            facts: None,
            batch: 0,
        },
        &mut |_, _| {},
    );
    assert_eq!(r.unwrap_err(), GraphError::Cancelled);
    assert!(!store.graph_visible(&active).unwrap());
    assert_eq!(
        store.graph_status(Some(&active)).unwrap().state,
        GraphLifecycle::Failed
    );
    drop(store);
    assert_eq!(
        f.count("SELECT COUNT(*) FROM relationships WHERE build_id = ?1"),
        0
    );
}

#[test]
fn only_symbol_dependents_are_re_resolved_and_the_result_matches_a_full_build() {
    let mut f = fixture();
    f.index();
    let total = f.count("SELECT COUNT(*) FROM files WHERE build_id = ?1") as usize;

    // A new file that defines nothing anybody references: only it is
    // re-resolved.
    common::write(
        &f.root,
        "python/pkg/lonely.py",
        "def lonely_xyz():\n    return 0\n",
    );
    f.index_base();
    let g = f.graph().unwrap();
    assert!(!g.full);
    assert_eq!(g.files_reresolved, 1, "{g:?}");

    // A new definition of a referenced symbol: its referencing (unchanged)
    // files are re-resolved too, and resolve to it.
    common::write(
        &f.root,
        "python/pkg/late.py",
        "def unknown_function():\n    return 0\n",
    );
    f.index_base();
    let g = f.graph().unwrap();
    assert!(
        g.files_reresolved > 1 && g.files_reresolved < total,
        "{g:?}"
    );
    assert!(g.resolutions_changed > 0, "{g:?}");
    let incremental = relationships_dump(&f);

    let (mut store, _) = f.store();
    store
        .put_graph_state_with_files(&docmonk_storage::graph::GraphState::default())
        .unwrap();
    drop(store);
    assert!(f.graph().unwrap().full);
    assert_eq!(relationships_dump(&f), incremental);
}

impl Fixture {
    fn index_base_with_facts(&mut self, cache: &std::sync::Arc<docmonk_storage::facts::FactCache>) {
        run_source(
            &self.layout,
            &mut self.cp,
            &self.src,
            &docmonk_code::registry_with_facts(Some(cache.clone())),
            &opts(),
            &mut NoProgress,
        )
        .unwrap();
    }

    fn graph_with_facts(
        &self,
        cache: &docmonk_storage::facts::FactCache,
    ) -> Result<GraphReport, GraphError> {
        let (mut store, active) = self.store();
        build_graph(
            &mut store,
            &self.root,
            &active,
            &GraphOptions {
                workers: 3,
                linker: None,
                cancel: None,
                phase: None,
                facts: Some(cache),
                batch: 0,
            },
            &mut |_, _| {},
        )
    }
}

fn fact_cache(f: &Fixture) -> std::sync::Arc<docmonk_storage::facts::FactCache> {
    let dir = f.root.parent().unwrap().join("cache");
    std::sync::Arc::new(
        docmonk_storage::facts::FactCache::open(&dir.join("graph-facts.db")).unwrap(),
    )
}

/// The same snapshot's graph derived from scratch, without any cache.
fn uncached_oracle(f: &Fixture) -> BTreeMap<String, String> {
    let (mut store, _) = f.store();
    store
        .put_graph_state_with_files(&docmonk_storage::graph::GraphState::default())
        .unwrap();
    drop(store);
    let g = f.graph().unwrap();
    assert!(g.full && g.counters.facts_hits == 0);
    relationships_dump(f)
}

fn append(f: &Fixture, rel: &str, text: &str) -> String {
    let p = f.root.join(rel);
    let before = std::fs::read_to_string(&p).unwrap();
    std::fs::write(&p, format!("{before}{text}")).unwrap();
    before
}

#[test]
fn cached_graph_facts_replace_the_second_parse_and_yield_the_same_graph() {
    let mut f = fixture();
    let cache = fact_cache(&f);
    f.index_base_with_facts(&cache);
    let g = f.graph_with_facts(&cache).unwrap();
    let c = &g.counters;
    assert!(c.files_derived > 0, "{g:?}");
    assert_eq!(
        (c.facts_hits, c.facts_misses),
        (c.files_derived, 0),
        "{g:?}"
    );
    assert_eq!(g.timings.parse_seconds, 0.0, "nothing was parsed again");
    let cached = relationships_dump(&f);
    assert_eq!(cached, uncached_oracle(&f));

    // An edit: indexing stores the new content's facts, the graph stage
    // uses them and drops the superseded entry.
    append(
        &f,
        "python/animals.py",
        "\ndef added_after():\n    return speak()\n",
    );
    f.index_base_with_facts(&cache);
    let g = f.graph_with_facts(&cache).unwrap();
    assert!(!g.full, "{g:?}");
    assert_eq!(
        (g.counters.facts_hits, g.counters.facts_misses),
        (1, 0),
        "{g:?}"
    );
    assert_eq!(g.counters.facts_dropped, 1, "{g:?}");
    let cached = relationships_dump(&f);
    assert_eq!(cached, uncached_oracle(&f));
}

#[test]
fn missing_or_unusable_facts_fall_back_to_parsing_and_never_hide_a_change() {
    let mut f = fixture();
    let cache = fact_cache(&f);
    // An index made without the cache (an existing source after turning
    // the cache on): every file falls back to parsing, nothing reindexes.
    f.index_base();
    let g = f.graph_with_facts(&cache).unwrap();
    assert_eq!(g.counters.facts_hits, 0, "{g:?}");
    assert_eq!(g.counters.facts_misses, g.counters.files_derived, "{g:?}");

    // Facts stored for an edit, then the file changes again on disk
    // before the graph stage: the bytes no longer match the published
    // hash, so the graph is stale whatever the cache holds.
    let original = append(&f, "python/animals.py", "\n# edited\n");
    f.index_base_with_facts(&cache);
    let indexed = std::fs::read_to_string(f.root.join("python/animals.py")).unwrap();
    append(&f, "python/animals.py", "\n# again\n");
    match f.graph_with_facts(&cache) {
        Err(GraphError::Stale(reason)) => assert!(reason.contains("python/animals.py"), "{reason}"),
        other => panic!("expected a stale graph, got {other:?}"),
    }
    assert_ne!(indexed, original);
    std::fs::write(f.root.join("python/animals.py"), &indexed).unwrap();
    let g = f.graph_with_facts(&cache).unwrap();
    assert_eq!(
        (g.counters.facts_hits, g.counters.facts_misses),
        (1, 0),
        "{g:?}"
    );
    let cached = relationships_dump(&f);
    assert_eq!(cached, uncached_oracle(&f));
}

/// Deterministic pseudo-random numbers (no dependency, stable seeds).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as usize) % n.max(1)
    }
}

/// One generated module: functions `f<k>` that call functions of other
/// modules by bare and qualified name (shared names make ambiguity).
fn generated_module(rng: &mut Lcg, modules: usize, defs: &[usize]) -> String {
    let mut s = String::new();
    for d in defs {
        let callee = rng.next(8);
        let module = rng.next(modules);
        let body = match rng.next(3) {
            0 => format!("f{callee}()"),
            1 => format!("gen.m{module}.f{callee}()"),
            _ => "0".to_owned(),
        };
        s.push_str(&format!("def f{d}():\n    return {body}\n\n"));
    }
    s
}

#[test]
fn random_change_sequences_resolve_incrementally_exactly_like_a_full_build() {
    for seed in [7_u64, 1_234, 99_991] {
        let mut f = fixture();
        let mut rng = Lcg(seed);
        let modules = 6;
        let mut defs: Vec<Vec<usize>> = (0..modules)
            .map(|_| (0..3).map(|_| rng.next(8)).collect())
            .collect();
        let write = |f: &Fixture, rng: &mut Lcg, m: usize, defs: &[usize]| {
            common::write(
                &f.root,
                &format!("gen/m{m}.py"),
                &generated_module(rng, modules, defs),
            );
        };
        for (m, d) in defs.iter().enumerate() {
            write(&f, &mut rng, m, d);
        }
        f.index();
        for step in 0..8 {
            let m = rng.next(modules);
            match rng.next(4) {
                // Add, remove or rename a definition.
                0 => defs[m].push(rng.next(8)),
                1 if defs[m].len() > 1 => {
                    let i = rng.next(defs[m].len());
                    defs[m].remove(i);
                }
                2 => {
                    let i = rng.next(defs[m].len());
                    defs[m][i] = rng.next(8);
                }
                // Body-only edit: same definitions, new calls.
                _ => {}
            }
            if step == 5 {
                // Delete a module outright, then bring it back next step.
                std::fs::remove_file(f.root.join(format!("gen/m{m}.py"))).unwrap();
            } else {
                write(&f, &mut rng, m, &defs[m]);
            }
            f.index_base();
            let g = f.graph().unwrap();
            assert!(!g.full, "seed {seed} step {step}: {g:?}");
            let incremental = relationships_dump(&f);
            assert_eq!(
                incremental,
                uncached_oracle(&f),
                "seed {seed} step {step}: incremental resolution diverged"
            );
        }
    }
}
