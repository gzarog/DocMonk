//! Selective relationship updates over a deterministic multi-source corpus:
//! a ring of declared dependencies (so every source is in a cycle), extra
//! fan-out edges, a symbol defined in every source (colliding names; the
//! source-local resolver always wins for it),
//! references that never resolve, and documents mentioning code. Every
//! scenario is checked against a clean full derivation of the same snapshot
//! (normalized semantic tuples; ids are content-addressed).
//!
//! `selective_updates_over_a_small_corpus` runs by default; the 150-source
//! variant (`cargo test -p docmonk-service --test selective_updates --
//! --ignored`) exercises the same assertions at scale. Neither measures
//! time: no performance claim is made by these tests.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use docmonk_code::graph_stage::PlanAction;
use docmonk_config::RagMonkConfig;
use docmonk_core::paths::{project_id_for_canonical, Home};
use docmonk_service::dependencies::DependencyOutcome;
use docmonk_service::indexing::{index_sources, SourceEvent};
use docmonk_storage::control::SourceRecord;
use docmonk_storage::knowledge::ProjectStore;
use docmonk_storage::StorageLayout;

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

fn api(i: usize, method: &str, body: &str) -> String {
    format!(
        "namespace P{i} {{\n  public class Api {{\n    public static int {method}() {{ {body} }}\n    public static int Helper() {{ return {i}; }}\n  }}\n}}\n"
    )
}

/// Producers of source `i` in a corpus of `n`: the next source (a ring, so
/// every source is in a cycle) and, for even `i`, the one after it.
fn producers(i: usize, n: usize) -> Vec<usize> {
    let mut p = vec![(i + 1) % n];
    if i.is_multiple_of(2) && n > 2 {
        p.push((i + 2) % n);
    }
    p
}

struct Corpus {
    _d: tempfile::TempDir,
    home: Home,
    roots: Vec<PathBuf>,
    ids: Vec<String>,
}

fn corpus(n: usize) -> Corpus {
    let d = tempfile::tempdir().unwrap();
    let home = Home::new(d.path().join("home"));
    home.ensure_layout().unwrap();
    let mut roots = Vec::new();
    for i in 0..n {
        let r = d.path().join(format!("s{i:03}"));
        write(&r, "Api.cs", &api(i, &format!("Get{i}"), "return 1;"));
        // Defined in every source: always resolved locally, never across.
        write(
            &r,
            "Common.cs",
            "namespace Common {\n  public class Util {\n    public static int Same() { return 0; }\n  }\n}\n",
        );
        let calls: String = producers(i, n)
            .iter()
            .map(|p| format!("P{p}.Api.Get{p}() + "))
            .collect();
        write(
            &r,
            "Program.cs",
            &format!(
                "namespace App{i} {{\n  public class Program {{\n    public static int Main() {{ return {calls}Common.Util.Same() + Missing.Thing.Call(); }}\n  }}\n}}\n"
            ),
        );
        write(
            &r,
            "docs/README.md",
            &format!("# Source {i}\n\nCall `Api.Get` or `Helper` from `Program`.\n"),
        );
        roots.push(r);
    }
    let mut cfg = RagMonkConfig::default();
    cfg.updates.enabled = false;
    cfg.indexing.relationship_dependencies = (0..n)
        .flat_map(|i| {
            producers(i, n).into_iter().map({
                let roots = &roots;
                move |p| {
                    format!(
                        "{} -> {}",
                        roots[i].to_str().unwrap(),
                        roots[p].to_str().unwrap()
                    )
                }
            })
        })
        .collect();
    docmonk_config::write_user_config(&cfg, &home).unwrap();
    let mut cat = docmonk_service::sources::catalog(&home).unwrap();
    let mut ids = Vec::new();
    for r in &roots {
        let s = cat.add(r.to_str().unwrap(), vec![], vec![]).unwrap();
        ids.push(s.id);
    }
    Corpus {
        _d: d,
        home,
        roots,
        ids,
    }
}

#[derive(Default, Debug)]
struct Run {
    graphs: BTreeMap<usize, docmonk_code::graph_stage::GraphReport>,
    deps: BTreeMap<usize, DependencyOutcome>,
}

impl Corpus {
    fn index(&self, id: &str) -> usize {
        self.ids.iter().position(|x| x == id).unwrap()
    }

    fn sources(&self) -> Vec<SourceRecord> {
        docmonk_service::sources::catalog(&self.home)
            .unwrap()
            .list(false)
            .unwrap()
    }

    /// Indexes `only` (every source when `None`).
    fn run(&self, only: Option<&[usize]>) -> Run {
        let all = self.sources();
        let sources: Vec<SourceRecord> = match only {
            None => all,
            Some(only) => only
                .iter()
                .map(|i| all.iter().find(|s| s.id == self.ids[*i]).unwrap().clone())
                .collect(),
        };
        let mut run = Run::default();
        let summary = index_sources(&self.home, &sources, "index", |e| match e {
            SourceEvent::Relationships { source, outcome } => {
                let g = outcome.run().unwrap_or_else(|| panic!("{outcome:?}"));
                run.graphs.insert(self.index(&source.id), g.graph.clone());
            }
            SourceEvent::Dependencies { source, outcome } => {
                assert!(
                    run.deps
                        .insert(self.index(&source.id), outcome.clone())
                        .is_none(),
                    "each consumer is planned at most once per run"
                );
            }
            SourceEvent::Failed { source, error } | SourceEvent::Blocked { source, error } => {
                panic!("{}: {}", source.id, error.message())
            }
            _ => {}
        })
        .unwrap();
        assert_eq!(summary.relationships.not_current(), 0);
        run
    }

    fn store(&self, i: usize) -> (ProjectStore, String) {
        let cp = docmonk_service::sources::control_plane(&self.home).unwrap();
        let b = cp.state(&self.ids[i]).unwrap().active_build_id.unwrap();
        let path = docmonk_core::paths::resolve(&self.roots[i]).unwrap();
        let s = ProjectStore::open(
            &StorageLayout::new(&self.home),
            &project_id_for_canonical(path.to_str().unwrap()),
            &self.ids[i],
            8,
        )
        .unwrap();
        (s, b)
    }

    fn graph(&self, i: usize) -> (Vec<String>, Vec<String>) {
        let (s, b) = self.store(i);
        assert!(s.graph_visible(&b).unwrap());
        let mut edges = Vec::new();
        for f in s.files(&b).unwrap() {
            for r in s.file_relationships(&b, &f.id).unwrap() {
                edges.push(format!(
                    "{}|{:?}|{:?}|{}|{}",
                    r.id, r.target_entity_id, r.target_symbol, r.resolver, r.confidence
                ));
            }
        }
        let mut links: Vec<String> = s
            .links(&b)
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

    /// Source `i`'s graph derived from scratch (replaces its graph: the
    /// last step of a scenario).
    fn oracle(&self, i: usize) -> (Vec<String>, Vec<String>) {
        let (mut s, b) = self.store(i);
        s.clear_graph(&b).unwrap();
        s.put_graph_state_with_files(&Default::default()).unwrap();
        let r = docmonk_knowledge::build_graph(&mut s, &self.roots[i], &b, 1, None, &mut |_, _| {})
            .unwrap();
        assert!(r.full);
        let mut edges = Vec::new();
        for f in s.files(&b).unwrap() {
            for r in s.file_relationships(&b, &f.id).unwrap() {
                edges.push(format!(
                    "{}|{:?}|{:?}|{}|{}",
                    r.id, r.target_entity_id, r.target_symbol, r.resolver, r.confidence
                ));
            }
        }
        let mut links: Vec<String> = s
            .links(&b)
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

    fn consumers_of(&self, p: usize) -> BTreeSet<usize> {
        let n = self.ids.len();
        (0..n).filter(|i| producers(*i, n).contains(&p)).collect()
    }
}

fn graph_work(r: &docmonk_code::graph_stage::GraphReport) -> usize {
    r.files_processed + r.files_reresolved + r.relationships_written + r.links
}

fn check(n: usize) {
    let c = corpus(n);

    // Initial: every graph initialized, every consumer bound.
    let run = c.run(None);
    assert_eq!(run.graphs.len(), n);
    assert!(run.graphs.values().all(|g| g.action == PlanAction::Full));
    assert_eq!(run.deps.len(), n, "every source is a consumer");
    for (i, o) in &run.deps {
        assert!(
            matches!(o, DependencyOutcome::Updated { bound, .. } if *bound == producers(*i, n).len()),
            "{i}: {o:?}"
        );
    }

    // Unchanged: no graph work anywhere, every consumer current.
    let run = c.run(None);
    assert!(
        run.graphs
            .values()
            .all(|g| g.action == PlanAction::Skip && graph_work(g) == 0),
        "{run:?}"
    );
    assert!(run.deps.values().all(|o| *o == DependencyOutcome::Current));

    // One body edit, indexing only that source: one file derived, no
    // consumer re-resolved anywhere, every consumer current.
    let edited = 3 % n;
    write(
        &c.roots[edited],
        "Api.cs",
        &api(edited, &format!("Get{edited}"), "var x = 41; return x + 1;"),
    );
    let run = c.run(Some(&[edited]));
    let g = &run.graphs[&edited];
    assert_eq!(g.action, PlanAction::Incremental);
    assert_eq!((g.files_processed, g.files_reresolved), (1, 1), "{g:?}");
    // Its consumers are current; only its own bindings (its graph changed)
    // are recomputed.
    for (i, o) in &run.deps {
        if *i != edited {
            assert_eq!(*o, DependencyOutcome::Current, "{i}: {run:?}");
        }
    }

    // A referenced definition renamed: exactly its consumers are updated
    // (against their existing bases), every other source untouched.
    write(
        &c.roots[edited],
        "Api.cs",
        &api(edited, "Renamed", "return 1;"),
    );
    let run = c.run(Some(&[edited]));
    let updated: BTreeSet<usize> = run
        .deps
        .iter()
        .filter(|(_, o)| matches!(o, DependencyOutcome::Updated { .. }))
        .map(|(i, _)| *i)
        .collect();
    let mut expected = c.consumers_of(edited);
    expected.insert(edited);
    assert_eq!(updated, expected, "{run:?}");
    for i in &updated {
        let (s, _) = c.store(*i);
        let m = s.external_manifest().unwrap().unwrap();
        assert!(!m
            .bindings
            .iter()
            .any(|b| b.reference_text == format!("P{edited}.Api.Get{edited}")));
    }

    // A document-only edit: only that document is derived.
    let doc = 5 % n;
    write(
        &c.roots[doc],
        "docs/README.md",
        &format!("# Source {doc}\n\nOnly `Helper` now.\n"),
    );
    let run = c.run(Some(&[doc]));
    let g = &run.graphs[&doc];
    assert_eq!(
        (
            g.action,
            g.files_processed,
            g.files_reresolved,
            g.resolutions_changed
        ),
        (PlanAction::Incremental, 1, 1, 0),
        "only the document itself: {g:?}"
    );

    // Every incremental graph equals a clean full derivation.
    for i in 0..n {
        let incremental = c.graph(i);
        assert_eq!(incremental, c.oracle(i), "source {i}");
    }
}

#[test]
fn selective_updates_over_a_small_corpus() {
    check(8);
}

#[test]
#[ignore = "150 sources: slow in debug builds; run with --ignored"]
fn selective_updates_over_150_sources() {
    check(150);
}
