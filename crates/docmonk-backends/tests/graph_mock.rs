//! Server graph generations through a deterministic in-memory engine:
//! invisible writes, one fenced promotion, refusal against a superseded
//! base build, and cleanup of failed generations. The base build pointer
//! is never touched by a graph publication.

use std::sync::{Arc, Mutex};

use docmonk_backends::bulk::BulkLimits;
use docmonk_backends::engine::Engine;
use docmonk_backends::error::Result as BResult;
use docmonk_backends::graph::{GraphPublishInput, ServerGraph};
use docmonk_backends::transport::{Counting, Method, Response, Transport};
use docmonk_backends::{BackendError, ServerBackend};
use docmonk_storage::control::IndexVersions;
use docmonk_storage::knowledge::{FileKnowledge, FileRow, ProjectStore, RelationshipRow};
use docmonk_storage::StorageLayout;
use serde_json::{json, Value};

#[derive(Default)]
struct Engine0 {
    state: Value,
    seq: i64,
    /// `(build ids, record_kind, file_id, source)` of every record.
    records: Vec<(Vec<String>, String, String, Value)>,
    fail_bulk: bool,
    /// Publishes another base build when the graph records are refreshed
    /// (a concurrent base publication between write and promotion).
    switch_base_on_refresh: Option<String>,
}

#[derive(Clone, Default)]
struct Mock(Arc<Mutex<Engine0>>);

fn ok(v: Value) -> BResult<Response> {
    Ok(Response {
        status: 200,
        body: serde_json::to_vec(&v).unwrap(),
    })
}

impl Transport for Mock {
    fn send(&self, method: Method, path: &str, body: Option<(&[u8], &str)>) -> BResult<Response> {
        let mut e = self.0.lock().unwrap();
        let base = path.split('?').next().unwrap();
        if base.starts_with("/rm-source-state/_doc/") {
            if method == Method::Get {
                return ok(json!({"_source": e.state, "_seq_no": e.seq, "_primary_term": 1}));
            }
            let want = format!("if_seq_no={}", e.seq);
            if !path.contains(&want) {
                return Ok(Response {
                    status: 409,
                    body: b"conflict".to_vec(),
                });
            }
            e.state = serde_json::from_slice(body.unwrap().0).unwrap();
            e.seq += 1;
            return ok(json!({"result": "updated"}));
        }
        if base == "/_bulk" {
            if e.fail_bulk {
                return Ok(Response {
                    status: 500,
                    body: b"bulk down".to_vec(),
                });
            }
            let text = String::from_utf8(body.unwrap().0.to_vec()).unwrap();
            for line in text.lines().filter(|l| !l.is_empty()) {
                let v: Value = serde_json::from_str(line).unwrap();
                if let (Some(b), Some(k)) = (v["build_id"].as_str(), v["record_kind"].as_str()) {
                    let f = v["file_id"].as_str().unwrap_or_default().to_owned();
                    e.records
                        .push((vec![b.to_owned()], k.to_owned(), f, v.clone()));
                }
            }
            return ok(json!({"errors": false, "items": []}));
        }
        if base.ends_with("/_refresh") {
            if let Some(b) = e.switch_base_on_refresh.take() {
                e.state["active_build_id"] = json!(b);
                e.seq += 1;
            }
            return ok(json!({}));
        }
        if base == "/rm-relationships/_search" {
            let v: Value = serde_json::from_slice(body.unwrap().0).unwrap();
            let filter = v["query"]["bool"]["filter"].as_array().unwrap().clone();
            let build = filter[1]["term"]["build_id"].as_str().unwrap().to_owned();
            let hits: Vec<Value> = e
                .records
                .iter()
                .filter(|(b, _, _, _)| b.contains(&build))
                .map(|(_, _, _, d)| json!({"_source": d, "sort": [d["relationship_id"]]}))
                .collect();
            return ok(json!({"hits": {"hits": hits}}));
        }
        if base.ends_with("/_update_by_query") {
            let v: Value = serde_json::from_slice(body.unwrap().0).unwrap();
            let b = v["script"]["params"]["b"].as_str().unwrap().to_owned();
            let filter = v["query"]["bool"]["filter"].as_array().unwrap().clone();
            let from = filter[1]["term"]["build_id"].as_str().unwrap().to_owned();
            if let Some(keep) = v["script"]["params"]["keep"].as_array() {
                // Copy-forward: matching records gain `b`.
                assert!(base.starts_with("/rm-relationships/"), "{base}");
                let keep: Vec<String> = keep
                    .iter()
                    .map(|k| k.as_str().unwrap().to_owned())
                    .collect();
                let files: Vec<String> = filter[2]["terms"]["file_id"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|f| f.as_str().unwrap().to_owned())
                    .collect();
                let mut updated = 0;
                for (builds, _, f, _) in &mut e.records {
                    if builds.contains(&from) && files.contains(f) {
                        builds.retain(|x| keep.contains(x));
                        if !builds.contains(&b) {
                            builds.push(b.clone());
                        }
                        updated += 1;
                    }
                }
                return ok(json!({"updated": updated, "failures": []}));
            }
            let before = e.records.len();
            for (builds, _, _, _) in &mut e.records {
                builds.retain(|x| *x != b);
            }
            e.records.retain(|(builds, _, _, _)| !builds.is_empty());
            return ok(json!({"deleted": before - e.records.len(), "failures": []}));
        }
        panic!("unexpected request {method:?} {path}");
    }
}

fn backend(m: &Mock) -> ServerBackend {
    ServerBackend::with_client(
        Counting::new(Box::new(m.clone())),
        Engine::OpenSearch,
        "rm",
        None,
        BulkLimits {
            max_actions: 100,
            max_bytes: 1 << 20,
            max_retries: 0,
            base_backoff: std::time::Duration::ZERO,
        },
    )
}

/// A staged store with one published build holding 3 edges.
fn staged(dir: &std::path::Path) -> (ProjectStore, String) {
    let layout = StorageLayout::at(dir);
    let mut s = ProjectStore::open(&layout, "p", "s1", 8).unwrap();
    let build = "local-1".to_owned();
    let v = IndexVersions {
        parser_version: "p".into(),
        chunker_version: "c".into(),
        converter_version: "v".into(),
        embedding_model_id: None,
        embedding_text_version: None,
    };
    s.create_build(&build, true, &v).unwrap();
    let file = FileRow {
        id: "f1".into(),
        rel_path: "a.py".into(),
        kind: "code".into(),
        status: "indexed".into(),
        ..FileRow::default()
    };
    s.put_files(&build, &[(&file, &FileKnowledge::default())])
        .unwrap();
    let rows: Vec<RelationshipRow> = (0..3)
        .map(|i| RelationshipRow {
            id: format!("r{i}"),
            file_id: "f1".into(),
            relationship_type: "calls".into(),
            source_entity_id: "e".into(),
            resolver: "same_file_name".into(),
            confidence: "exact".into(),
            ..RelationshipRow::default()
        })
        .collect();
    s.put_relationships(&build, &rows).unwrap();
    s.mark_published(&build).unwrap();
    (s, build)
}

fn input<'a>(
    store: &'a ProjectStore,
    local: &'a str,
    base: &'a str,
    generation: &'a str,
) -> GraphPublishInput<'a> {
    GraphPublishInput {
        source_id: "s1",
        store,
        local_build: local,
        base_build_id: base,
        lease: None,
        generation,
        input_digest: Some("digest-1"),
        manifest: None,
    }
}

fn setup() -> (Mock, ServerBackend, tempfile::TempDir) {
    let m = Mock::default();
    m.0.lock().unwrap().state = json!({
        "source_id": "s1", "state": "ready", "active_build_id": "b1",
        "versions": { "parser_version": "p" },
    });
    let b = backend(&m);
    (m, b, tempfile::tempdir().unwrap())
}

fn edges_of(m: &Mock, generation: &str) -> usize {
    m.0.lock()
        .unwrap()
        .records
        .iter()
        .filter(|(b, k, _, _)| b.iter().any(|x| x == generation) && k == "edge")
        .count()
}

#[test]
fn a_graph_generation_is_promoted_atomically_without_touching_the_base() {
    let (m, b, dir) = setup();
    let (store, local) = staged(dir.path());
    assert_eq!(b.visible_graph("s1", "b1").unwrap(), None);
    let r = b.publish_graph(&input(&store, &local, "b1", "g1")).unwrap();
    assert!(r.published);
    assert_eq!(r.edges_written, 3);
    assert_eq!(edges_of(&m, "g1"), 3);
    assert_eq!(b.visible_graph("s1", "b1").unwrap().as_deref(), Some("g1"));
    let st = m.0.lock().unwrap().state.clone();
    assert_eq!(st["active_build_id"], "b1", "base pointer untouched");
    assert_eq!(st["versions"]["parser_version"], "p");

    // Same graph again: nothing written, no new generation.
    let r = b.publish_graph(&input(&store, &local, "b1", "g2")).unwrap();
    assert!(!r.published);
    assert_eq!(r.generation.as_deref(), Some("g1"));
    assert_eq!(edges_of(&m, "g2"), 0);

    // A new generation replaces (and garbage-collects) the old one.
    let mut store = store;
    store
        .put_relationships(
            &local,
            &[RelationshipRow {
                id: "r9".into(),
                file_id: "f1".into(),
                relationship_type: "calls".into(),
                source_entity_id: "e".into(),
                resolver: "same_file_name".into(),
                confidence: "exact".into(),
                ..RelationshipRow::default()
            }],
        )
        .unwrap();
    let r = b.publish_graph(&input(&store, &local, "b1", "g3")).unwrap();
    assert!(r.published);
    assert_eq!(b.visible_graph("s1", "b1").unwrap().as_deref(), Some("g3"));
    assert_eq!(edges_of(&m, "g1"), 0, "retired generation collected");
    assert_eq!(edges_of(&m, "g3"), 4);
}

#[test]
fn a_graph_of_a_superseded_base_is_never_promoted() {
    let (m, b, dir) = setup();
    let (store, local) = staged(dir.path());
    // The base moved on before the publication started.
    m.0.lock().unwrap().state["active_build_id"] = json!("b2");
    let e = b
        .publish_graph(&input(&store, &local, "b1", "g1"))
        .unwrap_err();
    assert!(matches!(e, BackendError::Conflict(_)), "{e}");
    assert_eq!(edges_of(&m, "g1"), 0);

    // The base moves on between the write and the promotion.
    m.0.lock().unwrap().state["active_build_id"] = json!("b1");
    m.0.lock().unwrap().switch_base_on_refresh = Some("b3".into());
    let e = b
        .publish_graph(&input(&store, &local, "b1", "g2"))
        .unwrap_err();
    assert!(matches!(e, BackendError::Conflict(_)), "{e}");
    assert_eq!(
        edges_of(&m, "g2"),
        0,
        "the unpromoted generation is removed"
    );
    assert_eq!(b.visible_graph("s1", "b3").unwrap(), None);
    assert_eq!(b.visible_graph("s1", "b1").unwrap(), None);
}

#[test]
fn a_failed_generation_is_never_visible_and_a_republished_base_makes_the_graph_stale() {
    let (m, b, dir) = setup();
    let (store, local) = staged(dir.path());
    b.publish_graph(&input(&store, &local, "b1", "g1")).unwrap();

    m.0.lock().unwrap().fail_bulk = true;
    let mut store = store;
    store
        .put_relationships(
            &local,
            &[RelationshipRow {
                id: "r7".into(),
                file_id: "f1".into(),
                relationship_type: "calls".into(),
                source_entity_id: "e".into(),
                resolver: "same_file_name".into(),
                confidence: "exact".into(),
                ..RelationshipRow::default()
            }],
        )
        .unwrap();
    assert!(b.publish_graph(&input(&store, &local, "b1", "g2")).is_err());
    let (g, active) = b.graph("s1").unwrap().unwrap();
    assert_eq!(g.state, "failed");
    assert!(g.last_error.is_some());
    assert_eq!(b.visible_graph("s1", "b1").unwrap(), None);
    assert_eq!(edges_of(&m, "g2"), 0);
    assert_eq!(active.as_deref(), Some("b1"));

    // Recovery, then a base publication: the graph reports stale.
    m.0.lock().unwrap().fail_bulk = false;
    b.publish_graph(&input(&store, &local, "b1", "g3")).unwrap();
    m.0.lock().unwrap().state["active_build_id"] = json!("b2");
    let (g, active): (ServerGraph, _) = b.graph("s1").unwrap().unwrap();
    assert_eq!(g.effective_state(active.as_deref()), "stale");
    assert_eq!(b.visible_graph("s1", "b2").unwrap(), None);
}

fn edge(id: &str, file: &str) -> RelationshipRow {
    RelationshipRow {
        id: id.into(),
        file_id: file.into(),
        relationship_type: "calls".into(),
        source_entity_id: "e".into(),
        resolver: "same_file_name".into(),
        confidence: "exact".into(),
        ..RelationshipRow::default()
    }
}

#[test]
fn unchanged_files_are_copied_forward_and_only_changed_files_are_written() {
    let (m, b, dir) = setup();
    let (mut store, local) = staged(dir.path());
    // A second file with its own edges.
    let file2 = FileRow {
        id: "f2".into(),
        rel_path: "b.py".into(),
        kind: "code".into(),
        status: "indexed".into(),
        ..FileRow::default()
    };
    store
        .put_files(&local, &[(&file2, &FileKnowledge::default())])
        .unwrap();
    store
        .put_relationships(&local, &[edge("s0", "f2"), edge("s1", "f2")])
        .unwrap();
    let r = b.publish_graph(&input(&store, &local, "b1", "g1")).unwrap();
    assert_eq!(
        (r.files_written, r.files_copied, r.edges_written),
        (2, 0, 5)
    );

    // Only f1 changes: f2's two edges are copied, f1's four are written.
    store
        .put_relationships(&local, &[edge("r9", "f1")])
        .unwrap();
    let r = b.publish_graph(&input(&store, &local, "b1", "g2")).unwrap();
    assert!(r.published);
    assert_eq!((r.files_written, r.files_copied), (1, 1), "{r:?}");
    assert_eq!((r.edges_written, r.records_copied), (4, 2), "{r:?}");
    assert_eq!(edges_of(&m, "g2"), 6);
    assert_eq!(b.visible_graph("s1", "b1").unwrap().as_deref(), Some("g2"));
    // The retired generation is collected; shared records survive.
    assert_eq!(edges_of(&m, "g1"), 0);
    let n = m.0.lock().unwrap().records.len();
    assert_eq!(n, 6, "no duplicated records");

    // A new base with the same graph: everything is copied, nothing written.
    m.0.lock().unwrap().state["active_build_id"] = json!("b2");
    let r = b.publish_graph(&input(&store, &local, "b2", "g3")).unwrap();
    assert!(r.published);
    assert_eq!(
        (r.files_written, r.files_copied, r.edges_written),
        (0, 2, 0)
    );
    assert_eq!(b.visible_graph("s1", "b2").unwrap().as_deref(), Some("g3"));
    assert_eq!(edges_of(&m, "g3"), 6);
}

#[test]
fn an_equivalent_new_base_rebinds_the_generation_without_writing_records() {
    let (m, b, dir) = setup();
    let (store, local) = staged(dir.path());
    b.publish_graph(&input(&store, &local, "b1", "g1")).unwrap();
    let written = m.0.lock().unwrap().records.len();
    m.0.lock().unwrap().state["active_build_id"] = json!("b2");
    assert_eq!(b.visible_graph("s1", "b2").unwrap(), None);
    // Other inputs: refused (the caller derives and publishes instead).
    assert!(!b.rebind_graph("s1", None, "g1", "b2", "other").unwrap());
    assert!(!b.rebind_graph("s1", None, "g0", "b2", "digest-1").unwrap());
    // A superseded base: a conflict, nothing rebound.
    let e = b
        .rebind_graph("s1", None, "g1", "b1", "digest-1")
        .unwrap_err();
    assert!(matches!(e, BackendError::Conflict(_)), "{e}");
    assert!(b.rebind_graph("s1", None, "g1", "b2", "digest-1").unwrap());
    assert_eq!(b.visible_graph("s1", "b2").unwrap().as_deref(), Some("g1"));
    assert_eq!(
        m.0.lock().unwrap().records.len(),
        written,
        "no record written"
    );
}

#[test]
fn the_manifest_and_records_of_a_generation_can_be_read_back_by_another_host() {
    let (_m, b, dir) = setup();
    let (store, local) = staged(dir.path());
    let keys: std::collections::BTreeMap<String, String> =
        [("f1".to_owned(), "code:indexed:h".to_owned())].into();
    let mut i = input(&store, &local, "b1", "g1");
    i.manifest = Some(docmonk_backends::graph::GraphManifest {
        derivation: "d1",
        version: 1,
        file_keys: &keys,
    });
    b.publish_graph(&i).unwrap();
    let (g, _) = b.graph("s1").unwrap().unwrap();
    assert_eq!(g.file_keys, keys);
    assert_eq!(g.derivation.as_deref(), Some("d1"));
    assert_eq!(g.manifest_version, 1);
    assert_eq!(g.input_digest.as_deref(), Some("digest-1"));
    let records = b.graph_records("s1", "g1").unwrap();
    let mut ids: Vec<_> = records.edges.iter().map(|r| r.id.clone()).collect();
    ids.sort();
    assert_eq!(ids, ["r0", "r1", "r2"]);
    assert!(records.edges.iter().all(|r| r.file_id == "f1"));
}
