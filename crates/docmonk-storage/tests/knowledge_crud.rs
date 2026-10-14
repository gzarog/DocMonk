//! Local-mode CRUD/FTS and build visibility on the knowledge store.

use docmonk_core::ids::record;
use docmonk_core::models::SourceType;
use docmonk_core::paths::Home;
use docmonk_storage::control::{plan_for, ControlPlane, IndexVersions, NewSource, RebuildPlan};
use docmonk_storage::knowledge::{
    AttachmentProvenance, ChunkRow, DocumentRow, EntityRow, FileKnowledge, FileRow, LinkRow,
    ProjectStore, RelationshipRow,
};
use docmonk_storage::StorageLayout;

fn versions() -> IndexVersions {
    IndexVersions {
        parser_version: "p".into(),
        chunker_version: "c".into(),
        converter_version: "d".into(),
        embedding_model_id: Some("m".into()),
        embedding_text_version: Some("e".into()),
    }
}

fn file(source: &str, rel: &str, hash: &str) -> FileRow {
    FileRow {
        id: record::file_id(source, rel),
        rel_path: rel.into(),
        kind: if rel.ends_with(".py") {
            "code"
        } else {
            "document"
        }
        .into(),
        size: 10,
        mtime: 1.5,
        content_hash: Some(hash.into()),
        status: "indexed".into(),
        parser_version: Some("p".into()),
        chunker_version: Some("c".into()),
        converter_version: Some("d".into()),
        embedding_model_id: Some("m".into()),
        embedding_text_version: Some("e".into()),
        last_error: None,
        ..FileRow::default()
    }
}

fn code_knowledge(f: &FileRow, name: &str) -> FileKnowledge {
    let caller = record::entity_id(&f.id, "function", &format!("mod.{name}"), 0);
    let callee = record::entity_id(&f.id, "function", "mod.helper", 0);
    let entity = |id: &str, n: &str, line: i64| EntityRow {
        id: id.into(),
        file_id: f.id.clone(),
        kind: "function".into(),
        name: n.into(),
        qualified_name: format!("mod.{n}"),
        language: "python".into(),
        parent_id: None,
        signature: Some(format!("def {n}()")),
        start_line: line,
        end_line: line + 1,
        ..EntityRow::default()
    };
    FileKnowledge {
        entities: vec![entity(&caller, name, 1), entity(&callee, "helper", 5)],
        relationships: vec![RelationshipRow {
            id: record::relationship_id(&caller, "calls", &callee, 0),
            file_id: f.id.clone(),
            relationship_type: "calls".into(),
            source_entity_id: caller.clone(),
            target_entity_id: Some(callee),
            target_symbol: Some("helper".into()),
            resolver: "same_file_name".into(),
            confidence: "exact".into(),
            source_location: Some(format!("{}:2", f.rel_path)),
            evidence: Some("helper()".into()),
            reference_text: Some("helper".into()),
        }],
        ..FileKnowledge::default()
    }
}

fn email_knowledge(f: &FileRow) -> FileKnowledge {
    let parent = record::document_id(&f.id, None);
    let child = record::document_id(&f.id, Some(1));
    let doc = |id: &str, title: &str, attachment: Option<AttachmentProvenance>| DocumentRow {
        id: id.into(),
        file_id: f.id.clone(),
        format: if attachment.is_some() { "txt" } else { "eml" }.into(),
        title: Some(title.into()),
        author: None,
        page_count: None,
        is_scanned: false,
        content_hash: None,
        attachment,
    };
    let chunk = |doc_id: &str, ord: i64, text: &str| ChunkRow {
        id: record::chunk_id(doc_id, ord as u64),
        document_id: doc_id.into(),
        file_id: f.id.clone(),
        kind: "paragraph".into(),
        ordinal: ord,
        heading_path: vec!["Quarterly".into()],
        heading_level: None,
        text: text.into(),
        search_text: text.into(),
        embedding_text: None,
        token_count: Some(4),
        page_start: None,
        page_end: None,
        table_rows: None,
        caption: None,
        parent_ordinal: None,
    };
    FileKnowledge {
        documents: vec![
            doc(&parent, "Budget mail", None),
            doc(
                &child,
                "notes",
                Some(AttachmentProvenance {
                    parent_document_id: parent.clone(),
                    name: Some("notes.txt".into()),
                    content_type: Some("text/plain".into()),
                    index: 1,
                    content_id: None,
                }),
            ),
        ],
        chunks: vec![
            chunk(&parent, 0, "please review the budget"),
            chunk(&child, 0, "zebra herd notes"),
        ],
        ..FileKnowledge::default()
    }
}

#[test]
fn builds_are_invisible_until_published_and_incremental_carry_forward_works() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::new(tmp.path());
    let layout = StorageLayout::new(&home);
    let mut cp = ControlPlane::open(&layout, 8).unwrap();
    let (src, _) = cp
        .add_source(&NewSource {
            canonical_path: "/r".into(),
            source_type: SourceType::Local,
            enabled: true,
            include_patterns: vec![],
            exclude_patterns: vec![],
        })
        .unwrap();
    let mut store = ProjectStore::open(&layout, "proj", &src.id, 8).unwrap();

    // Full build b1.
    cp.begin_build(&src.id, "b1").unwrap();
    store.create_build("b1", true, &versions()).unwrap();
    let a = file(&src.id, "a.py", "h1");
    let mail = file(&src.id, "mail.eml", "h2");
    store
        .put_file("b1", &a, &code_knowledge(&a, "alpha"))
        .unwrap();
    store
        .put_file("b1", &mail, &email_knowledge(&mail))
        .unwrap();
    // Re-putting a file replaces rather than duplicates.
    store
        .put_file("b1", &a, &code_knowledge(&a, "alpha"))
        .unwrap();
    assert_eq!(store.count("entities", "b1").unwrap(), 2);
    assert_eq!(store.count("relationships", "b1").unwrap(), 1);

    // Not yet published: readers resolve the active build, which is none.
    assert_eq!(cp.state(&src.id).unwrap().active_build_id, None);

    cp.publish_build(&src.id, "b1", &versions(), true).unwrap();
    store.mark_published("b1").unwrap();
    let active = cp.state(&src.id).unwrap().active_build_id.unwrap();
    assert_eq!(active, "b1");
    assert_eq!(store.search_code(&active, "alpha", 10).unwrap().len(), 1);
    assert_eq!(store.search_paths(&active, "mail", 10).unwrap().len(), 1);
    let hits = store.search_chunks(&active, "zebra", 10).unwrap();
    assert_eq!(hits.len(), 1);
    // Attachment provenance survives storage.
    let docs = store.documents(&active).unwrap();
    assert_eq!(docs.len(), 2);
    let child = docs.iter().find(|d| d.attachment.is_some()).unwrap();
    assert_eq!(
        child.attachment.as_ref().unwrap().name.as_deref(),
        Some("notes.txt")
    );
    assert_eq!(
        child.attachment.as_ref().unwrap().parent_document_id,
        record::document_id(&mail.id, None)
    );
    // Hostile FTS input is quoted, not interpreted.
    assert!(store
        .search_chunks(&active, "\"unbalanced AND OR (", 10)
        .unwrap()
        .is_empty());

    let links = vec![LinkRow {
        id: record::link_id(
            &code_knowledge(&a, "alpha").entities[0].id,
            &docs[0].id,
            None,
            "mentioned_in",
            "exact_name",
        ),
        link_type: "mentioned_in".into(),
        entity_id: code_knowledge(&a, "alpha").entities[0].id.clone(),
        document_id: docs[0].id.clone(),
        chunk_id: None,
        resolver: "exact_name".into(),
        confidence: "high".into(),
        evidence: None,
    }];
    store.put_links(&active, &links).unwrap();
    store.put_links(&active, &links).unwrap();
    assert_eq!(store.count("cross_links", &active).unwrap(), 1);

    // Incremental build b2: a.py changed, mail.eml unchanged.
    let plan = plan_for(&cp.state(&src.id).unwrap(), &versions());
    assert_eq!(
        plan,
        RebuildPlan::Incremental {
            active_build_id: "b1".into()
        }
    );
    let reusable = store.reusable_files(&plan).unwrap();
    assert_eq!(reusable.len(), 2);
    cp.begin_build(&src.id, "b2").unwrap();
    store.create_build("b2", false, &versions()).unwrap();
    store.carry_forward("b1", "b2", &mail.id).unwrap();
    let a2 = file(&src.id, "a.py", "h3");
    store
        .put_file("b2", &a2, &code_knowledge(&a2, "beta"))
        .unwrap();
    // While b2 builds, searches on the active build still see b1 only.
    assert_eq!(store.search_code("b1", "beta", 10).unwrap().len(), 0);
    assert_eq!(store.search_code("b1", "alpha", 10).unwrap().len(), 1);

    cp.publish_build(&src.id, "b2", &versions(), false).unwrap();
    store.mark_published("b2").unwrap();
    assert_eq!(store.search_code("b2", "beta", 10).unwrap().len(), 1);
    assert_eq!(store.search_code("b2", "alpha", 10).unwrap().len(), 0);
    assert_eq!(
        store.search_chunks("b2", "zebra", 10).unwrap().len(),
        1,
        "carried forward with FTS"
    );
    assert_eq!(store.documents("b2").unwrap().len(), 2);

    // GC removes the superseded build entirely.
    assert_eq!(store.gc_builds(Some("b2")).unwrap(), 1);
    assert_eq!(store.count("entities", "b1").unwrap(), 0);
    assert_eq!(store.search_chunks("b1", "zebra", 10).unwrap().len(), 0);
    assert_eq!(store.search_chunks("b2", "zebra", 10).unwrap().len(), 1);

    // Deleting a file in a later build.
    store.remove_file("b2", &a2.id).unwrap();
    assert_eq!(store.count("files", "b2").unwrap(), 1);
    assert_eq!(store.search_code("b2", "beta", 10).unwrap().len(), 0);
}

#[test]
fn jobs_claim_retry_fail_and_recover() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = StorageLayout::new(&Home::new(tmp.path()));
    let mut store = ProjectStore::open(&layout, "p", "src_x", 8).unwrap();
    store.create_build("b", true, &versions()).unwrap();
    let j1 = store.enqueue("b", "f1", "a.py", 0).unwrap();
    let j2 = store.enqueue("b", "f2", "b.py", 5).unwrap();
    assert_eq!(
        store.enqueue("b", "f1", "a.py", 0).unwrap(),
        j1,
        "enqueue is idempotent"
    );
    assert_eq!(store.queue_depth("b").unwrap(), 2);
    let first = store.claim_next("b").unwrap().unwrap();
    assert_eq!(first.id, j2, "higher priority first");
    store
        .fail_job(&j2, "E", "boom", Some("9999-01-01T00:00:00Z"), false)
        .unwrap();
    let next = store.claim_next("b").unwrap().unwrap();
    assert_eq!(next.id, j1, "retry not due yet");
    assert!(store.claim_next("b").unwrap().is_none());
    assert_eq!(
        store.recover_stuck().unwrap(),
        1,
        "crash recovery requeues processing jobs"
    );
    let again = store.claim_next("b").unwrap().unwrap();
    assert_eq!(again.id, j1);
    store.complete_job(&j1).unwrap();
    store.fail_job(&j2, "E", "fatal", None, true).unwrap();
    assert_eq!(store.queue_depth("b").unwrap(), 0);
    store
        .record_error(Some("b"), Some("f2"), Some("b.py"), "E", "fatal")
        .unwrap();
}

#[test]
fn fresh_home_creates_current_schema_and_refuses_anything_else() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = StorageLayout::new(&Home::new(tmp.path()));
    // A fresh home: both databases are created directly where expected.
    ControlPlane::open(&layout, 8).unwrap();
    ProjectStore::open(&layout, "p", "s", 8).unwrap();
    assert!(tmp.path().join("state").join("control.db").is_file());
    assert!(tmp
        .path()
        .join("projects")
        .join("p")
        .join("knowledge.db")
        .is_file());
    // Reopening a current database is a no-op.
    ProjectStore::open(&layout, "p", "s", 8).unwrap();

    // A database with any other schema is refused and left as it was.
    let foreign = layout.project_db("q");
    {
        let conn = docmonk_storage::db::open(&foreign, 8).unwrap();
        conn.execute_batch("CREATE TABLE files (id TEXT); INSERT INTO files VALUES ('x');")
            .unwrap();
    }
    let err = ProjectStore::open(&layout, "q", "s", 8).err().unwrap();
    assert!(err.to_string().contains("docmonk index"), "{err}");
    let conn = rusqlite::Connection::open(&foreign).unwrap();
    let tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tables, 1, "nothing was created or altered");
}

#[test]
fn set_based_carry_forward_copies_everything_but_excluded_files() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = StorageLayout::new(&Home::new(tmp.path()));
    let mut store = ProjectStore::open(&layout, "p", "s", 8).unwrap();
    store.create_build("b1", true, &versions()).unwrap();
    let a = file("s", "a.py", "1");
    let b = file("s", "b.py", "2");
    let mail = file("s", "m.eml", "3");
    store
        .put_file("b1", &a, &code_knowledge(&a, "alpha"))
        .unwrap();
    store
        .put_file("b1", &b, &code_knowledge(&b, "bravo"))
        .unwrap();
    store
        .put_file("b1", &mail, &email_knowledge(&mail))
        .unwrap();
    store.create_build("b2", false, &versions()).unwrap();
    let n = store
        .carry_forward_except(
            "b1",
            "b2",
            std::slice::from_ref(&b.id),
            &[(a.id.clone(), 99, 9.5)],
        )
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(store.count("entities", "b2").unwrap(), 2);
    assert_eq!(store.count("documents", "b2").unwrap(), 2);
    assert_eq!(store.search_code("b2", "alpha", 5).unwrap().len(), 1);
    assert!(store.search_code("b2", "bravo", 5).unwrap().is_empty());
    assert_eq!(store.search_chunks("b2", "zebra", 5).unwrap().len(), 1);
    let files = store.files("b2").unwrap();
    let ra = files.iter().find(|f| f.id == a.id).unwrap();
    assert_eq!((ra.size, ra.mtime), (99, 9.5));
}

#[test]
fn vector_blobs_round_trip_little_endian() {
    use docmonk_storage::vectors::{decode_vector, encode_vector};
    let v = vec![0.0f32, -1.5, 3.25e-7, f32::MAX];
    let blob = encode_vector(&v);
    assert_eq!(blob.len(), 16);
    assert_eq!(&blob[4..8], &(-1.5f32).to_le_bytes());
    assert_eq!(decode_vector(&blob), v);
}

#[test]
fn relationships_stream_by_file_in_one_statement_and_per_file_reads_use_the_file_index() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = StorageLayout::new(&Home::new(tmp.path()));
    let mut store = ProjectStore::open(&layout, "p", "s", 8).unwrap();
    store.create_build("b1", true, &versions()).unwrap();
    // Ids interleave across files so id order differs from file order.
    let rows: Vec<RelationshipRow> = (0..30)
        .map(|i| RelationshipRow {
            id: format!("r{:02}", (i * 7) % 30),
            file_id: format!("f{}", i % 4),
            relationship_type: "calls".into(),
            source_entity_id: "e".into(),
            resolver: "pending".into(),
            confidence: "medium".into(),
            reference_text: Some(format!("t{i}")),
            ..RelationshipRow::default()
        })
        .collect();
    store.put_relationships("b1", &rows).unwrap();

    let before = docmonk_storage::stats::sql_statements();
    let mut groups: Vec<(String, Vec<RelationshipRow>)> = Vec::new();
    store
        .for_each_file_relationships("b1", |file, rows| {
            groups.push((file, rows));
            Ok::<(), docmonk_storage::StorageError>(())
        })
        .unwrap();
    assert_eq!(docmonk_storage::stats::sql_statements() - before, 1);
    let files: Vec<&str> = groups.iter().map(|(f, _)| f.as_str()).collect();
    assert_eq!(files, ["f0", "f1", "f2", "f3"]);
    for (file, rows) in &groups {
        assert!(rows.iter().all(|r| &r.file_id == file));
        assert_eq!(rows, &store.file_relationships("b1", file).unwrap());
    }
    assert_eq!(groups.iter().map(|(_, r)| r.len()).sum::<usize>(), 30);
    let mut none = 0;
    store
        .for_each_file_relationships("other", |_, _| {
            none += 1;
            Ok::<(), docmonk_storage::StorageError>(())
        })
        .unwrap();
    assert_eq!(none, 0);
}
