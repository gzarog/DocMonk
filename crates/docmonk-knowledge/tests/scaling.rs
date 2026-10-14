//! Deterministic guards for the relationship graph's cost model: counts,
//! never timings, so they are stable in CI.
//!
//! On a generated C# + Markdown corpus at two sizes, every kind of edit
//! (body edit, definition rename, file add/remove, document edit) must
//! yield exactly the graph a from-scratch build yields (canonical
//! fingerprint of every edge and link field), and a one-file edit must
//! issue the same number of SQL statements whatever the source size: a
//! per-file or per-name query loop (an N+1 pattern) shows up here as a
//! count that grows with the corpus.

mod common;

use std::path::PathBuf;

use docmonk_code::graph_stage::GraphReport;
use docmonk_core::paths::project_id_for_canonical;
use docmonk_indexing::coordinator::{run_source, NoProgress, Options};
use docmonk_knowledge::canonical::{fingerprint, GraphFingerprint};
use docmonk_storage::control::{ControlPlane, SourceRecord};
use docmonk_storage::knowledge::ProjectStore;
use docmonk_storage::StorageLayout;

fn controller(i: usize, n: usize) -> String {
    format!(
        "namespace Shop.Controllers\n{{\n    public class Orders{i}Controller\n    {{\n        \
         private readonly IOrderService{i} _service;\n        \
         [HttpGet(\"api/orders{i}/{{id}}\")]\n        \
         public object Get(int id) {{ return Mapper{dep}.ToDto(_service.Find(id)); }}\n    }}\n}}\n",
        dep = (i + 1) % n
    )
}

fn service(i: usize, find: &str, body: &str) -> String {
    format!(
        "namespace Shop.Services\n{{\n    public interface IOrderService{i} {{ object {find}(int id); }}\n    \
         public class OrderService{i} : IOrderService{i}\n    {{\n        \
         public object {find}(int id) {{ {body} }}\n    }}\n    \
         public static class Mapper{i} {{ public static object ToDto(object o) {{ return o; }} }}\n}}\n"
    )
}

fn note(d: usize, n: usize, extra: &str) -> String {
    let mut s = format!("# Note {d}\n\n{extra}");
    for k in 0..4 {
        let i = (d * 7 + k * 3) % n;
        s.push_str(&format!(
            "## Orders {i}\n\nOrders{i}Controller calls OrderService{i}.Find \
             (see OrderService{i}.cs) for GET api/orders{i}/{{id}}.\n\n"
        ));
    }
    s
}

struct Corpus {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    layout: StorageLayout,
    cp: ControlPlane,
    src: SourceRecord,
    groups: usize,
}

impl Corpus {
    fn new(groups: usize) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("corpus");
        for i in 0..groups {
            common::write(
                &root,
                &format!("src/Orders{i}Controller.cs"),
                &controller(i, groups),
            );
            common::write(
                &root,
                &format!("src/OrderService{i}.cs"),
                &service(i, "Find", "return null;"),
            );
        }
        for d in 0..groups / 4 {
            common::write(&root, &format!("docs/note{d}.md"), &note(d, groups, ""));
        }
        let home = common::home(tmp.path());
        let layout = StorageLayout::new(&home);
        let mut cp = common::control(&home);
        let src = common::add_source(&mut cp, &root, &[], &[]);
        Self {
            _tmp: tmp,
            root,
            layout,
            cp,
            src,
            groups,
        }
    }

    fn store(&self) -> (ProjectStore, String) {
        let active = self
            .cp
            .state(&self.src.id)
            .unwrap()
            .active_build_id
            .unwrap();
        let s = ProjectStore::open(
            &self.layout,
            &project_id_for_canonical(&self.src.path),
            &self.src.id,
            8,
        )
        .unwrap();
        (s, active)
    }

    /// Both stages, like `docmonk index`; returns the graph report.
    fn index(&mut self) -> GraphReport {
        let cfg = docmonk_config::DocMonkConfig::default();
        run_source(
            &self.layout,
            &mut self.cp,
            &self.src,
            &docmonk_convert::registry(&cfg),
            &Options::from_config(&cfg),
            &mut NoProgress,
        )
        .unwrap();
        let (mut store, active) = self.store();
        docmonk_knowledge::build_graph(&mut store, &self.root, &active, 2, None, &mut |_, _| {})
            .unwrap()
    }

    fn graph(&self) -> GraphFingerprint {
        let (store, active) = self.store();
        fingerprint(&store, &active).unwrap()
    }

    /// The same snapshot's graph derived from scratch.
    fn oracle(&self) -> GraphFingerprint {
        let (mut store, active) = self.store();
        store
            .put_graph_state_with_files(&docmonk_storage::graph::GraphState::default())
            .unwrap();
        let r = docmonk_knowledge::build_graph(
            &mut store,
            &self.root,
            &active,
            2,
            None,
            &mut |_, _| {},
        )
        .unwrap();
        assert!(r.full);
        fingerprint(&store, &active).unwrap()
    }
}

/// Applies each edit kind in turn; returns `(edit, report)` after checking
/// the incremental graph against a from-scratch build.
fn edits(c: &mut Corpus) -> Vec<(&'static str, GraphReport)> {
    let cold = c.index();
    assert!(cold.full && cold.links > 0, "{cold:?}");
    let mut out = Vec::new();
    let mut step = |c: &mut Corpus, name: &'static str| {
        let r = c.index();
        assert!(!r.full, "{name}: {r:?}");
        let incremental = c.graph();
        assert_eq!(
            incremental,
            c.oracle(),
            "{name}: incremental graph diverged"
        );
        out.push((name, r));
    };
    common::write(
        &c.root,
        "src/OrderService0.cs",
        &service(0, "Find", "var x = 1; return null;"),
    );
    step(c, "body_edit");
    common::write(
        &c.root,
        "src/OrderService1.cs",
        &service(1, "FindById", "return null;"),
    );
    step(c, "rename_definition");
    common::write(
        &c.root,
        "src/Extra.cs",
        "namespace Shop.Extra\n{\n    public class Report { public void Run() { Mapper0.ToDto(null); } }\n}\n",
    );
    step(c, "add_file");
    std::fs::remove_file(c.root.join("src/OrderService2.cs")).unwrap();
    step(c, "remove_file");
    let groups = c.groups;
    common::write(
        &c.root,
        "docs/note0.md",
        &note(
            0,
            groups,
            "Report.Run and Orders3Controller are new here.\n\n",
        ),
    );
    step(c, "doc_edit");
    out
}

#[test]
fn incremental_graphs_equal_full_builds_and_one_file_costs_do_not_grow_with_the_source() {
    let mut small = Corpus::new(12);
    let mut large = Corpus::new(48);
    let small = edits(&mut small);
    let large = edits(&mut large);
    for ((name, s), (_, l)) in small.iter().zip(&large) {
        let (s, l) = (&s.counters, &l.counters);
        // Fixed per-build bookkeeping is allowed a little slack; anything
        // per file or per name of the source is not.
        assert!(
            l.sql_statements <= s.sql_statements + 8,
            "{name}: {} statements at 4x the files vs {}",
            l.sql_statements,
            s.sql_statements
        );
        assert!(
            l.resolution.candidate_queries <= 4,
            "{name}: {:?}",
            l.resolution
        );
    }
    let body = &large[0].1;
    assert_eq!(body.files_processed, 1, "{body:?}");
    assert!(body.linking.narrow, "a code-only edit loads only its file");
    let doc = &large[4].1;
    assert!(!doc.linking.narrow && doc.links > 0, "{doc:?}");
}
