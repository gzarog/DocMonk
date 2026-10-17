//! Real-cluster acceptance tests for the server index schema.
//!
//! Set `DOCMONK_TEST_OPENSEARCH_URL` / `DOCMONK_TEST_ELASTICSEARCH_URL` to a
//! disposable test cluster. With `DOCMONK_REQUIRE_LIVE_SERVER=1` a missing
//! URL is a failure instead of a skip (CI). The scenario creates and deletes
//! uniquely named indexes, so never point these variables at a real
//! deployment.

use std::time::Duration;

use docmonk_backends::backend::{ChunkDoc, DocumentDoc, EdgeDoc, EntityDoc, FileDoc, LinkDoc};
use docmonk_backends::bulk::BulkLimits;
use docmonk_backends::engine::{Engine, VectorSpec};
use docmonk_backends::schema::{IndexKind, IndexSettings};
use docmonk_backends::transport::{Auth, Client, Counting, HttpTransport, Method};
use docmonk_backends::{BackendError, ServerBackend};
use docmonk_core::ids::record;
use serde_json::json;

fn url(var: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(u) if !u.is_empty() => Some(u),
        _ => {
            assert!(
                std::env::var("DOCMONK_REQUIRE_LIVE_SERVER").as_deref() != Ok("1"),
                "{var} is required when DOCMONK_REQUIRE_LIVE_SERVER=1"
            );
            eprintln!("skipping: {var} not set");
            None
        }
    }
}

fn client(base: &str) -> Client {
    Counting::new(Box::new(
        HttpTransport::new(base, Auth::None, Duration::from_secs(60), true).unwrap(),
    ))
}

fn unique(tag: &str) -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("rmtest{tag}{}", n % 1_000_000_000)
}

fn vector(dims: u32) -> VectorSpec {
    VectorSpec {
        dims,
        m: 16,
        ef_construction: 100,
        model_id: "test-model".into(),
    }
}

fn backend(
    base: &str,
    engine: Engine,
    prefix: &str,
    dims: u32,
    max_actions: usize,
) -> ServerBackend {
    ServerBackend::with_client(
        client(base),
        engine,
        prefix,
        Some(vector(dims)),
        BulkLimits {
            max_actions,
            max_bytes: 5_000_000,
            max_retries: 3,
            base_backoff: Duration::from_millis(50),
        },
    )
    .with_settings(IndexSettings {
        shards: 1,
        replicas: 0,
    })
}

fn delete_index(c: &Client, name: &str) {
    let _ = c.send(Method::Delete, &format!("/{name}"), None);
}

/// Writes a synthetic source with `files` code files and one email with an
/// attachment child document. Returns the number of chunks written.
fn write_corpus(b: &ServerBackend, source: &str, build: &str, files: usize, marker: &str) -> usize {
    let g = b
        .pending_generation(docmonk_backends::generations::Line::Base, source, build)
        .unwrap();
    let mut w = b.build_writer(source, build, g);
    let mut chunks = 0;
    for i in 0..files {
        let rel = format!("pkg/mod_{i}.py");
        let file_id = record::file_id(source, &rel);
        w.file(&FileDoc {
            file_id: file_id.clone(),
            rel_path: rel.clone(),
            kind: "code".into(),
            size: 100,
            mtime: 1.0,
            content_hash: Some(format!("h{i}")),
            parser_version: Some("p".into()),
            chunker_version: Some("c".into()),
            converter_version: Some("d".into()),
            embedding_model_id: Some("test-model".into()),
            embedding_text_version: Some("e".into()),
            ..Default::default()
        })
        .unwrap();
        let caller =
            record::entity_id(&file_id, "function", &format!("pkg.mod_{i}.{marker}_fn"), 0);
        let callee = record::entity_id(&file_id, "function", &format!("pkg.mod_{i}.helper"), 0);
        for (id, name) in [
            (&caller, format!("{marker}_fn")),
            (&callee, "helper".to_string()),
        ] {
            w.entity(&EntityDoc {
                entity_id: id.clone(),
                file_id: file_id.clone(),
                rel_path: rel.clone(),
                kind: "function".into(),
                name: name.clone(),
                qualified_name: format!("pkg.mod_{i}.{name}"),
                language: "python".into(),
                parent_id: None,
                signature: Some(format!("def {name}()")),
                start_line: 1,
                end_line: 3,
                ..Default::default()
            })
            .unwrap();
        }
        w.edge(&EdgeDoc {
            relationship_id: record::relationship_id(&caller, "calls", &callee, 0),
            file_id: file_id.clone(),
            rel_path: rel.clone(),
            relationship_type: "calls".into(),
            source_entity_id: caller.clone(),
            target_entity_id: Some(callee.clone()),
            target_symbol: Some("helper".into()),
            resolver: "same_file_name".into(),
            confidence: "exact".into(),
            source_location: Some(format!("{rel}:2")),
            evidence: Some("helper()".into()),
            ..Default::default()
        })
        .unwrap();
    }
    // An email with one attachment child document.
    let rel = "mail/budget.eml".to_string();
    let file_id = record::file_id(source, &rel);
    let parent = record::document_id(&file_id, None);
    let child = record::document_id(&file_id, Some(1));
    for (doc_id, att) in [(&parent, None), (&child, Some(1))] {
        w.document(&DocumentDoc {
            document_id: doc_id.clone(),
            file_id: file_id.clone(),
            rel_path: rel.clone(),
            format: if att.is_some() { "txt" } else { "eml" }.into(),
            title: Some("Budget".into()),
            author: None,
            page_count: None,
            is_scanned: false,
            content_hash: None,
            parent_document_id: att.map(|_| parent.clone()),
            attachment_name: att.map(|_| "notes.txt".into()),
            attachment_content_type: att.map(|_| "text/plain".into()),
            attachment_index: att,
            attachment_content_id: None,
            ..Default::default()
        })
        .unwrap();
        for ord in 0..3u64 {
            let id = record::chunk_id(doc_id, ord);
            w.chunk(&ChunkDoc {
                chunk_id: id.clone(),
                document_id: doc_id.clone(),
                parent_document_id: att.map(|_| parent.clone()),
                file_id: file_id.clone(),
                rel_path: rel.clone(),
                kind: "paragraph".into(),
                ordinal: ord as i64,
                heading_path: vec!["Quarterly".into()],
                heading_level: None,
                title: Some("Budget".into()),
                search_text: format!("{marker} budget review paragraph {ord}"),
                text: format!("{marker} budget review paragraph {ord}"),
                embedding_text: None,
                token_count: Some(5),
                page_start: None,
                page_end: None,
                table_rows: None,
                caption: None,
                embedding: Some(vec![0.5, 0.5, 0.5, 0.5]),
                ..Default::default()
            })
            .unwrap();
            chunks += 1;
        }
    }
    w.link(&LinkDoc {
        relationship_id: record::link_id("e", &parent, None, "mentioned_in", "exact_name"),
        relationship_type: "mentioned_in".into(),
        entity_id: "e".into(),
        document_id: parent.clone(),
        chunk_id: None,
        file_id: file_id.clone(),
        rel_path: rel,
        resolver: "exact_name".into(),
        confidence: "high".into(),
        evidence: None,
    })
    .unwrap();
    w.finish().unwrap();
    chunks
}

fn scenario(base: &str, engine: Engine) {
    let prefix = unique("live");
    let c = client(base);

    // ---- a foreign index under the prefix is refused, never modified -----
    let foreign = unique("foreign");
    let r = c
        .send(
            Method::Put,
            &format!("/{foreign}-files"),
            Some((
                br#"{"mappings":{"properties":{"x":{"type":"keyword"}}}}"#,
                "application/json",
            )),
        )
        .unwrap();
    assert!(r.status < 300, "{}", String::from_utf8_lossy(&r.body));
    let err = backend(base, engine, &foreign, 4, 40).init().unwrap_err();
    assert!(err.to_string().contains("not a DocMonk index"), "{err}");
    let mapping = c
        .send(Method::Get, &format!("/{foreign}-files/_mapping"), None)
        .unwrap();
    let body = String::from_utf8_lossy(&mapping.body);
    assert!(!body.contains("_meta"), "foreign index untouched: {body}");
    for kind in [
        "files",
        "source-state",
        "code",
        "documents",
        "chunks",
        "relationships",
        "runtime",
    ] {
        delete_index(&c, &format!("{foreign}-{kind}"));
    }

    // ---- empty init -----------------------------------------------------------
    let b = backend(base, engine, &prefix, 4, 40);
    let init = b.init().unwrap();
    assert_eq!(init.created.len(), 7);
    assert!(b.init().unwrap().existing.len() == 7, "init is idempotent");
    assert!(b.check_schema().unwrap().is_empty());
    let wrong = backend(base, engine, &prefix, 8, 40);
    assert!(matches!(wrong.init(), Err(BackendError::SchemaMismatch(_))));

    // ---- full rebuild, invisible until published -------------------------
    let src = "src_live";
    let versions = json!({"schema_version": 1, "parser_version": "p"});
    b.begin_build(src, "/repo", "b1").unwrap();
    let before = b.stats();
    let chunks = write_corpus(&b, src, "b1", 120, "alpha");
    let after = b.stats();
    let actions = after.bulk_actions - before.bulk_actions;
    // 4 records per code file, 2 documents, the chunks and 1 link.
    assert_eq!(actions as usize, 120 * 4 + 2 + chunks + 1);
    let bulk_requests = after.bulk_requests - before.bulk_requests;
    assert!(
        bulk_requests as usize <= actions.div_ceil(40) as usize + 2,
        "bounded batches: {bulk_requests}"
    );
    assert_eq!(
        after.requests - before.requests,
        bulk_requests,
        "every record write went through _bulk"
    );
    assert!(
        after.max_bulk_actions <= 40,
        "action bound: {}",
        after.max_bulk_actions
    );
    assert!(
        after.max_bulk_bytes <= 5_000_000,
        "byte bound: {}",
        after.max_bulk_bytes
    );
    b.client()
        .send(Method::Post, &format!("/{prefix}-*/_refresh"), None)
        .unwrap();
    assert_eq!(
        b.raw_count(IndexKind::Chunks, src, "b1").unwrap(),
        chunks as u64
    );
    assert_eq!(
        b.count(IndexKind::Chunks, None).unwrap(),
        0,
        "unpublished build invisible"
    );
    assert!(b.search_chunks("alpha", 10).unwrap().is_empty());

    b.publish_build(src, "b1", &versions, true).unwrap();
    assert_eq!(
        b.count(IndexKind::Chunks, Some(src)).unwrap(),
        chunks as u64
    );
    assert_eq!(b.count(IndexKind::Code, Some(src)).unwrap(), 240);
    assert_eq!(b.count(IndexKind::Documents, Some(src)).unwrap(), 2);
    assert_eq!(b.count(IndexKind::Relationships, Some(src)).unwrap(), 121);
    assert!(!b.search_chunks("alpha budget", 10).unwrap().is_empty());
    assert!(!b.search_code("alpha_fn", 10).unwrap().is_empty());

    // Re-writing the same records into the same build is idempotent.
    b.begin_build(src, "/repo", "b2").unwrap();
    write_corpus(&b, src, "b2", 10, "beta");
    write_corpus(&b, src, "b2", 10, "beta");
    b.client()
        .send(Method::Post, &format!("/{prefix}-*/_refresh"), None)
        .unwrap();
    assert_eq!(b.raw_count(IndexKind::Code, src, "b2").unwrap(), 20);

    // ---- failure injection: b2 "crashes" (never published) --------------
    assert!(
        b.search_code("beta_fn", 10).unwrap().is_empty(),
        "crashed build invisible"
    );
    assert_eq!(
        b.count(IndexKind::Code, Some(src)).unwrap(),
        240,
        "previous build still served"
    );
    // A new build cleans up the abandoned one.
    b.begin_build(src, "/repo", "b3").unwrap();
    assert_eq!(b.raw_count(IndexKind::Code, src, "b2").unwrap(), 0);
    write_corpus(&b, src, "b3", 5, "gamma");
    b.abort_build(src, "b3").unwrap();
    assert_eq!(b.raw_count(IndexKind::Code, src, "b3").unwrap(), 0);
    let state = b.source_state(src).unwrap().unwrap();
    assert_eq!(
        (state.state.as_str(), state.active_build_id.as_deref()),
        ("failed", Some("b1"))
    );
    assert!(matches!(
        b.publish_build(src, "b3", &versions, false),
        Err(BackendError::Conflict(_))
    ));

    // ---- incremental publish garbage-collects the old build --------------
    b.begin_build(src, "/repo", "b4").unwrap();
    write_corpus(&b, src, "b4", 30, "delta");
    b.publish_build(src, "b4", &versions, false).unwrap();
    assert_eq!(b.count(IndexKind::Code, Some(src)).unwrap(), 60);
    assert_eq!(b.raw_count(IndexKind::Code, src, "b1").unwrap(), 0);
    assert!(b.search_code("alpha_fn", 10).unwrap().is_empty());
    assert!(!b.search_code("delta_fn", 10).unwrap().is_empty());

    // ---- build mode toggles and restores serving settings -----------------
    let mode = b.enter_build_mode().unwrap();
    let s = b
        .client()
        .send(
            Method::Get,
            &format!("/{prefix}-chunks/_settings?flat_settings=true"),
            None,
        )
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(
        s.pointer(&format!("/{prefix}-chunks/settings/index.refresh_interval"))
            .unwrap(),
        "-1"
    );
    mode.restore().unwrap();
    let s = b
        .client()
        .send(
            Method::Get,
            &format!("/{prefix}-chunks/_settings?include_defaults=true&flat_settings=true"),
            None,
        )
        .unwrap()
        .json()
        .unwrap();
    let refresh = s
        .pointer(&format!("/{prefix}-chunks/settings/index.refresh_interval"))
        .or_else(|| s.pointer(&format!("/{prefix}-chunks/defaults/index.refresh_interval")))
        .unwrap();
    assert_ne!(refresh, "-1");

    // ---- source removal ---------------------------------------------------
    assert!(b.remove_source(src).unwrap() > 0);
    assert_eq!(b.count(IndexKind::Code, None).unwrap(), 0);
    assert!(b.source_state(src).unwrap().is_none());

    for kind in IndexKind::ALL {
        delete_index(&c, &b.index(kind));
    }
}

#[test]
fn opensearch_lifecycle() {
    if let Some(u) = url("DOCMONK_TEST_OPENSEARCH_URL") {
        scenario(&u, Engine::OpenSearch);
    }
}

#[test]
fn elasticsearch_lifecycle() {
    if let Some(u) = url("DOCMONK_TEST_ELASTICSEARCH_URL") {
        scenario(&u, Engine::Elasticsearch);
    }
}

// ---------------------------------------------------------------- P0 ---

fn p0_backend(base: &str, engine: Engine, prefix: &str) -> ServerBackend {
    backend(base, engine, prefix, 4, 500)
}

fn drop_prefix(base: &str, prefix: &str) {
    let c = client(base);
    for kind in [
        "files",
        "source-state",
        "code",
        "documents",
        "chunks",
        "relationships",
        "runtime",
    ] {
        delete_index(&c, &format!("{prefix}-{kind}"));
    }
}

/// Catalog scans page with `search_after`: 10001 registrations come back
/// complete, unique and in order (P0-S01).
fn catalog_paging(base: &str, engine: Engine) {
    let prefix = unique("page");
    let b = p0_backend(base, engine, &prefix);
    b.init().unwrap();
    let index = format!("{prefix}-source-state");
    let n = 10_001;
    for chunk in (0..n).collect::<Vec<_>>().chunks(2_000) {
        let mut body = Vec::new();
        for i in chunk {
            let id = format!("src_{i:06}");
            body.extend_from_slice(
                json!({ "index": { "_index": index, "_id": id } })
                    .to_string()
                    .as_bytes(),
            );
            body.push(b'\n');
            body.extend_from_slice(
                json!({ "source_id": id, "path": format!("/r/{i}"), "enabled": i % 2 == 0,
                        "active_build_id": format!("b{i}"), "state": "ready" })
                .to_string()
                .as_bytes(),
            );
            body.push(b'\n');
        }
        let r = b
            .client()
            .send(
                Method::Post,
                "/_bulk?refresh=true",
                Some((&body, "application/x-ndjson")),
            )
            .unwrap();
        assert!(r.status < 300);
    }
    let all = b.list_catalog(false).unwrap();
    assert_eq!(all.len(), n);
    let mut ids: Vec<&str> = all.iter().map(|e| e.source_id.as_str()).collect();
    let sorted = ids.clone();
    ids.dedup();
    assert_eq!(ids.len(), n, "no duplicates");
    assert_eq!(ids, sorted, "stable total order");
    assert_eq!(b.list_catalog(true).unwrap().len(), n.div_ceil(2));
    assert_eq!(b.active_builds().unwrap().len(), n, "no 10000 truncation");
    drop_prefix(base, &prefix);
}

/// Writer leases fence stale holders; replaced builds stay readable for
/// the GC grace period (P0-S02).
fn leases_and_grace(base: &str, engine: Engine) {
    use docmonk_backends::backend::CatalogEntry;
    let prefix = unique("lease");
    let b = p0_backend(base, engine, &prefix).with_gc_grace(Duration::from_secs(3600));
    b.init().unwrap();
    let src = "src_lease";
    let (e, created) = b
        .register_source(&CatalogEntry {
            source_id: src.into(),
            path: "/repo".into(),
            source_type: "local".into(),
            enabled: true,
            include_patterns: vec!["*.rs".into()],
            exclude_patterns: vec![],
            created_at: String::new(),
            updated_at: String::new(),
        })
        .unwrap();
    assert!(created);
    assert_eq!(e.include_patterns, ["*.rs"]);
    let (_, again) = b
        .register_source(&CatalogEntry {
            source_id: src.into(),
            path: "/elsewhere".into(),
            source_type: "local".into(),
            enabled: true,
            include_patterns: vec![],
            exclude_patterns: vec![],
            created_at: String::new(),
            updated_at: String::new(),
        })
        .unwrap();
    assert!(!again, "registration is idempotent");

    let v = json!({"parser_version": "p"});
    let l1 = b
        .acquire_lease(src, "host-a:1", Duration::from_secs(60))
        .unwrap();
    assert!(
        b.acquire_lease(src, "host-b:2", Duration::from_secs(60))
            .is_err(),
        "a live lease excludes other owners"
    );
    b.begin_build_with(Some(&l1), src, "/repo", "g1").unwrap();
    write_corpus(&b, src, "g1", 5, "gamma");
    b.publish_build_with(Some(&l1), src, "g1", &v, true)
        .unwrap();
    b.release_lease(&l1).unwrap();

    // Host A takes the lease, starts g2, then stalls; its lease expires and
    // host B takes over and publishes g3. A cannot publish or abort g2 any
    // more, and B's state is intact.
    let la = b
        .acquire_lease(src, "host-a:1", Duration::from_millis(400))
        .unwrap();
    b.begin_build_with(Some(&la), src, "/repo", "g2").unwrap();
    write_corpus(&b, src, "g2", 5, "delta");
    std::thread::sleep(Duration::from_millis(600));
    assert!(matches!(
        b.publish_build_with(Some(&la), src, "g2", &v, false),
        Err(BackendError::Conflict(_))
    ));
    let lb = b
        .acquire_lease(src, "host-b:2", Duration::from_secs(60))
        .unwrap();
    assert!(lb.token > la.token, "fencing token increases");
    b.begin_build_with(Some(&lb), src, "/repo", "g3").unwrap();
    write_corpus(&b, src, "g3", 5, "epsilon");
    assert!(matches!(
        b.publish_build_with(Some(&la), src, "g3", &v, false),
        Err(BackendError::Conflict(_))
    ));
    assert!(b.abort_build_with(Some(&la), src, "g3", None).is_err());
    b.publish_build_with(Some(&lb), src, "g3", &v, false)
        .unwrap();
    assert_eq!(b.active_builds().unwrap()[src], "g3");
    // g1 was retired, not deleted: still readable within the grace period.
    assert!(b.raw_count(IndexKind::Code, src, "g1").unwrap() > 0);
    let st = b.source_state(src).unwrap().unwrap();
    assert_eq!(st.doc["retired_builds"][0]["build_id"], "g1");
    // Readers only see g3.
    assert!(b.search_code("epsilon_fn", 10).unwrap().len() == 5);
    assert!(b.search_code("gamma_fn", 10).unwrap().is_empty());
    // The catalog fields survived every lifecycle write.
    assert_eq!(
        b.catalog_entry(src).unwrap().unwrap().include_patterns,
        ["*.rs"]
    );
    b.release_lease(&lb).unwrap();
    drop_prefix(base, &prefix);
}

/// k-NN through the read port on both engines, with model-dimension
/// checks (P0-S03).
fn vectors(base: &str, engine: Engine) {
    use docmonk_backends::reader::ServerReader;
    use docmonk_storage::read::KnowledgeRead;
    let prefix = unique("vec");
    let b = std::sync::Arc::new(p0_backend(base, engine, &prefix));
    b.init().unwrap();
    let src = "src_vec";
    let g = b.begin_build(src, "/repo", "vecb1").unwrap();
    let mut w = b.build_writer(src, "vecb1", g);
    let file_id = record::file_id(src, "a.md");
    let doc = record::document_id(&file_id, None);
    for (i, v) in [
        [1.0f32, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.7, 0.7, 0.0, 0.0],
    ]
    .iter()
    .enumerate()
    {
        w.chunk(&ChunkDoc {
            chunk_id: format!("c{i}"),
            document_id: doc.clone(),
            file_id: file_id.clone(),
            rel_path: "a.md".into(),
            kind: "paragraph".into(),
            ordinal: i as i64,
            search_text: format!("chunk {i}"),
            text: format!("chunk {i}"),
            embedding: Some(v.to_vec()),
            embedding_fingerprint: Some("fp".into()),
            ..Default::default()
        })
        .unwrap();
    }
    w.finish().unwrap();
    b.publish_build(src, "vecb1", &json!({}), true).unwrap();
    let r = ServerReader::new(b.clone(), src);
    let hits = r
        .vector_search("vecb1", "fp", &[1.0, 0.1, 0.0, 0.0], 2, 0)
        .unwrap()
        .unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].1, "c0", "{hits:?}");
    assert!(hits[0].2 > 0.9);
    // Another model's vectors are never mixed in.
    assert!(r
        .vector_search("vecb1", "other-model", &[1.0, 0.0, 0.0, 0.0], 2, 0)
        .unwrap()
        .unwrap()
        .is_empty());
    // Wrong dimension: explicit error, no silent result.
    assert!(r.vector_search("vecb1", "fp", &[1.0, 0.0], 2, 0).is_err());
    // Another source's reader sees nothing of this build.
    let other = ServerReader::new(b.clone(), "src_other");
    assert!(other
        .vector_search("vecb1", "fp", &[1.0, 0.0, 0.0, 0.0], 2, 0)
        .unwrap()
        .unwrap()
        .is_empty());
    drop_prefix(base, &prefix);
}

/// A prefix created by another schema identity is refused with reset
/// instructions; nothing is converted or deleted (fresh-prefix policy).
fn schema_identity(base: &str, engine: Engine) {
    let prefix = unique("ident");
    let b = p0_backend(base, engine, &prefix);
    b.init().unwrap();
    let c = client(base);
    // The runtime index of this schema is missing: reported, not created.
    delete_index(&c, &format!("{prefix}-runtime"));
    assert_eq!(b.check_schema().unwrap(), vec![format!("{prefix}-runtime")]);
    // An index of another schema identity under the prefix.
    delete_index(&c, &format!("{prefix}-files"));
    let r = c
        .send(
            Method::Put,
            &format!("/{prefix}-files"),
            Some((
                br#"{"mappings":{"_meta":{"schema":"docmonk","schema_version":2,"index_kind":"files","vector":null},"properties":{"x":{"type":"keyword"}}}}"#,
                "application/json",
            )),
        )
        .unwrap();
    assert!(r.status < 300);
    let err = b.check_schema().unwrap_err();
    assert!(matches!(err, BackendError::SchemaMismatch(_)), "{err}");
    assert!(err.to_string().contains("fresh index_prefix"), "{err}");
    assert!(matches!(b.init(), Err(BackendError::SchemaMismatch(_))));
    let still = c
        .send(Method::Get, &format!("/{prefix}-files/_mapping"), None)
        .unwrap();
    assert!(String::from_utf8_lossy(&still.body).contains("\"schema_version\":2"));
    drop_prefix(base, &prefix);
}

#[test]
fn opensearch_schema_identity() {
    if let Some(u) = url("DOCMONK_TEST_OPENSEARCH_URL") {
        schema_identity(&u, Engine::OpenSearch);
    }
}

#[test]
fn elasticsearch_schema_identity() {
    if let Some(u) = url("DOCMONK_TEST_ELASTICSEARCH_URL") {
        schema_identity(&u, Engine::Elasticsearch);
    }
}

#[test]
fn opensearch_p0_catalog_leases_vectors() {
    if let Some(u) = url("DOCMONK_TEST_OPENSEARCH_URL") {
        catalog_paging(&u, Engine::OpenSearch);
        leases_and_grace(&u, Engine::OpenSearch);
        vectors(&u, Engine::OpenSearch);
    }
}

#[test]
fn elasticsearch_p0_catalog_leases_vectors() {
    if let Some(u) = url("DOCMONK_TEST_ELASTICSEARCH_URL") {
        catalog_paging(&u, Engine::Elasticsearch);
        leases_and_grace(&u, Engine::Elasticsearch);
        vectors(&u, Engine::Elasticsearch);
    }
}
