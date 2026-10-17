//! Base publication by generations (ADR 0035, audit F10) against the
//! in-memory engine: a one-file edit writes that file's records only and
//! closes only its previous ones; unchanged records are never rewritten;
//! the compare-and-swap is the only visibility switch, so failed, killed
//! and superseded attempts never show; readers pinned to a retired build
//! keep their snapshot through the grace window.

use docmonk_backends::bulk::BulkLimits;
use docmonk_backends::engine::Engine;
use docmonk_backends::memory::Memory;
use docmonk_backends::publish::PublishInput;
use docmonk_backends::schema::IndexKind;
use docmonk_backends::transport::Counting;
use docmonk_backends::ServerBackend;
use docmonk_storage::control::IndexVersions;
use docmonk_storage::knowledge::{EntityRow, FileKnowledge, FileRow, ProjectStore};
use docmonk_storage::StorageLayout;
use serde_json::json;

const FILES: &str = "rm-files";
const CODE: &str = "rm-code";

fn backend(m: &Memory) -> ServerBackend {
    ServerBackend::with_client(
        Counting::new(Box::new(m.clone())),
        Engine::OpenSearch,
        "rm",
        None,
        BulkLimits {
            max_actions: 1000,
            max_bytes: 8 << 20,
            max_retries: 0,
            base_backoff: std::time::Duration::ZERO,
        },
    )
}

/// A staged build: `(file id, content hash, entity count)` per file.
fn stage(dir: &std::path::Path, files: &[(&str, &str, usize)]) -> (ProjectStore, String) {
    let layout = StorageLayout::at(dir);
    let mut s = ProjectStore::open(&layout, "p", "s1", 8).unwrap();
    let build = "local".to_owned();
    let v = IndexVersions {
        parser_version: "p".into(),
        chunker_version: "c".into(),
        converter_version: "v".into(),
        embedding_model_id: None,
        embedding_text_version: None,
    };
    s.create_build(&build, true, &v).unwrap();
    for (id, hash, n) in files {
        let row = FileRow {
            id: (*id).into(),
            rel_path: format!("{id}.py"),
            kind: "code".into(),
            status: "indexed".into(),
            content_hash: Some((*hash).into()),
            ..FileRow::default()
        };
        let k = FileKnowledge {
            entities: (0..*n)
                .map(|i| EntityRow {
                    id: format!("{id}-e{i}"),
                    file_id: (*id).into(),
                    kind: "function".into(),
                    name: format!("f{i}"),
                    qualified_name: format!("{id}.f{i}"),
                    language: "python".into(),
                    start_line: i as i64,
                    end_line: i as i64,
                    ..EntityRow::default()
                })
                .collect(),
            ..FileKnowledge::default()
        };
        s.put_files(&build, &[(&row, &k)]).unwrap();
    }
    s.mark_published(&build).unwrap();
    (s, build)
}

struct Env {
    m: Memory,
    b: ServerBackend,
    dirs: Vec<tempfile::TempDir>,
}

impl Env {
    fn new() -> Self {
        let m = Memory::new();
        Env {
            b: backend(&m),
            m,
            dirs: Vec::new(),
        }
    }

    fn publish(
        &mut self,
        build: &str,
        files: &[(&str, &str, usize)],
    ) -> docmonk_backends::publish::PublishReport {
        let dir = tempfile::tempdir().unwrap();
        let (store, local) = stage(dir.path(), files);
        self.dirs.push(dir);
        self.b
            .publish_from_store(&PublishInput {
                source_id: "s1",
                path: "/src",
                store: &store,
                local_build: &local,
                versions: json!({ "parser_version": "p" }),
                full: false,
                lease: None,
                build_id: build,
            })
            .unwrap()
    }

    /// Visible `(files, entities)` of the source.
    fn visible(&self) -> (u64, u64) {
        (
            self.b.count(IndexKind::Files, Some("s1")).unwrap(),
            self.b.count(IndexKind::Code, Some("s1")).unwrap(),
        )
    }

    fn stored(&self, index: &str) -> usize {
        self.m.docs(index).len()
    }
}

fn corpus(edited: Option<&str>) -> Vec<(&'static str, &'static str, usize)> {
    let mut v: Vec<(&str, &str, usize)> = (0..50)
        .map(|i| {
            (
                Box::leak(format!("f{i:02}").into_boxed_str()) as &str,
                "h1",
                4,
            )
        })
        .collect();
    if let Some(id) = edited {
        for f in &mut v {
            if f.0 == id {
                f.1 = "h2";
                f.2 = 5;
            }
        }
    }
    v
}

#[test]
fn a_one_file_edit_rewrites_no_unchanged_record() {
    let mut e = Env::new();
    let first = e.publish("b1", &corpus(None));
    assert_eq!((first.files_written, first.files_kept), (50, 0));
    assert_eq!(e.visible(), (50, 200));

    let before = e.m.touched();
    let r = e.publish("b2", &corpus(Some("f07")));
    let t = e.m.touched();
    assert!(r.published);
    assert_eq!(
        (r.files_written, r.files_kept, r.files_closed),
        (1, 49, 1),
        "{r:?}"
    );
    // Written: the edited file's row and its 5 entities. Closed: its old
    // row and 4 entities. Nothing else is touched.
    assert_eq!(
        t.indexed - before.indexed,
        6,
        "only the changed file's records are written"
    );
    assert_eq!(
        t.updated - before.updated,
        5,
        "only the replaced records are closed"
    );
    assert_eq!(r.records_closed, 5);
    assert_eq!(e.visible(), (50, 201));
    // The replaced versions were collected (no grace): no duplicates.
    assert_eq!(e.stored(FILES), 50);
    assert_eq!(e.stored(CODE), 201);

    // A warm pass writes nothing at all.
    let before = e.m.touched();
    let r = e.publish("b3", &corpus(Some("f07")));
    assert!(!r.published);
    assert_eq!(e.m.touched(), before);
}

#[test]
fn deleted_files_disappear_and_new_files_appear() {
    let mut e = Env::new();
    e.publish("b1", &[("a", "h", 2), ("b", "h", 2)]);
    let r = e.publish("b2", &[("a", "h", 2), ("c", "h", 3)]);
    assert_eq!(
        (r.files_written, r.files_kept, r.files_closed),
        (1, 1, 1),
        "{r:?}"
    );
    assert_eq!(e.visible(), (2, 5));
    let paths: Vec<String> =
        e.b.search_code("x", 100)
            .unwrap()
            .into_iter()
            .map(|h| h.rel_path)
            .collect();
    assert!(!paths.iter().any(|p| p == "b.py"), "{paths:?}");
}

#[test]
fn a_failed_or_killed_attempt_is_never_visible() {
    let mut e = Env::new();
    e.publish("b1", &corpus(None));
    // A bulk failure aborts the attempt: the previous generation stays.
    e.m.fail_next_bulk(1);
    let dir = tempfile::tempdir().unwrap();
    let (store, local) = stage(dir.path(), &corpus(Some("f01")));
    let err = e.b.publish_from_store(&PublishInput {
        source_id: "s1",
        path: "/src",
        store: &store,
        local_build: &local,
        versions: json!({}),
        full: false,
        lease: None,
        build_id: "b2",
    });
    assert!(err.is_err());
    assert_eq!(e.visible(), (50, 200));

    // A writer killed after writing and closing, before the switch: its
    // new records and its closes have no effect on readers.
    let g = e.b.begin_build("s1", "/src", "b3").unwrap();
    let mut w = e.b.build_writer("s1", "b3", g);
    w.file(&docmonk_backends::backend::FileDoc {
        file_id: "f00".into(),
        rel_path: "f00.py".into(),
        ..Default::default()
    })
    .unwrap();
    w.finish().unwrap();
    e.b.refresh_all().unwrap();
    // (the close request the killed writer had sent)
    e.b.close_files_for_test("s1", &["f00".into(), "f01".into()], g)
        .unwrap();
    assert_eq!(
        e.visible(),
        (50, 200),
        "an unpublished attempt changes nothing"
    );

    // The next attempt aborts the killed one and publishes exactly its own
    // content: no duplicate, nothing lost.
    let r = e.publish("b4", &corpus(Some("f02")));
    assert!(r.published);
    assert_eq!(e.visible(), (50, 201));
    let st = e.b.source_state("s1").unwrap().unwrap();
    assert_eq!(st.active_build_id.as_deref(), Some("b4"));
    assert!(
        st.doc["mvcc"]["base"]["aborted"]
            .as_array()
            .is_some_and(|a| a.len() == 2),
        "{}",
        st.doc["mvcc"]
    );
}

#[test]
fn a_superseded_writer_cannot_change_what_readers_see() {
    let mut e = Env::new();
    e.publish("b1", &corpus(None));
    // Writer A begins, then loses the source to writer B, which publishes.
    let ga = e.b.begin_build("s1", "/src", "a").unwrap();
    let r = e.publish("b2", &corpus(Some("f03")));
    assert!(r.published);
    assert_eq!(e.visible(), (50, 201));
    // A keeps going: writes a file version and closes another file.
    let mut w = e.b.build_writer("s1", "a", ga);
    w.file(&docmonk_backends::backend::FileDoc {
        file_id: "zz".into(),
        rel_path: "zz.py".into(),
        ..Default::default()
    })
    .unwrap();
    w.finish().unwrap();
    e.b.refresh_all().unwrap();
    e.b.close_files_for_test("s1", &["f10".into()], ga).unwrap();
    assert_eq!(e.visible(), (50, 201), "the superseded attempt is aborted");
    // And A cannot publish.
    assert!(e.b.publish_build("s1", "a", &json!({}), false).is_err());
    assert_eq!(e.visible(), (50, 201));
}

#[test]
fn a_reader_pinned_to_a_retired_build_keeps_its_snapshot_until_the_grace_ends() {
    let m = Memory::new();
    let mut e = Env {
        b: backend(&m).with_gc_grace(std::time::Duration::from_secs(3600)),
        m,
        dirs: Vec::new(),
    };
    e.publish("b1", &corpus(None));
    e.publish("b2", &corpus(Some("f05")));
    let b = std::sync::Arc::new(e.b);
    let reader = docmonk_backends::reader::ServerReader::new(b.clone(), "s1");
    use docmonk_storage::read::KnowledgeRead;
    // f05 gained a fifth entity in b2.
    assert!(reader.entity("b1", "f05-e4").unwrap().is_none());
    assert!(reader.entity("b2", "f05-e4").unwrap().is_some());
    assert!(reader.entity("b1", "f05-e0").unwrap().is_some());
    assert!(reader.entity("b2", "f05-e0").unwrap().is_some());
    // Both versions are stored; each snapshot sees exactly one.
    assert_eq!(e.m.docs(CODE).len(), 205);
}
