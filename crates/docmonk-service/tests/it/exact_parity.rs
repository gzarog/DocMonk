//! Exact-occurrence parity between SQLite and the server read path
//! (P03/P06/P08): the same published graph, read locally and through
//! `ServerReader` over the deterministic in-memory engine, yields the same
//! code occurrences (class, path, line, matched text, resolver) and the
//! same capability. Server requests are exact `term` lookups on
//! `target_symbol`, never wildcard scans.
//!
//! Document verification needs full-text matching, which the in-memory
//! engine does not model; it is covered by the SQLite tests and the opt-in
//! live server tests.

// clean-slate-audit: allow-start (API URL paths with a version segment are user data)

use std::sync::{Arc, Mutex};

use docmonk_backends::bulk::BulkLimits;
use docmonk_backends::engine::Engine;
use docmonk_backends::graph::{GraphManifest, GraphPublishInput};
use docmonk_backends::memory::Memory;
use docmonk_backends::reader::ServerReader;
use docmonk_backends::transport::{Counting, Method, Response, Transport};
use docmonk_backends::ServerBackend;
use docmonk_core::models::SourceType;
use docmonk_core::paths::{project_id_for_canonical, Home};
use docmonk_indexing::coordinator::{run_source, NoProgress, Options};
use docmonk_retrieval::exact::{self, ExactOptions};
use docmonk_retrieval::route::extract_anchors;
use docmonk_retrieval::Corpus;
use docmonk_storage::control::{ControlPlane, NewSource};
use docmonk_storage::knowledge::ProjectStore;
use docmonk_storage::StorageLayout;
use serde_json::json;

/// The memory engine, recording every search body.
#[derive(Clone, Default)]
struct Recording {
    mem: Memory,
    bodies: Arc<Mutex<Vec<(String, String)>>>,
}

impl Transport for Recording {
    fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<(&[u8], &str)>,
    ) -> docmonk_backends::error::Result<Response> {
        if path.contains("_search") {
            self.bodies.lock().unwrap().push((
                path.to_owned(),
                body.map(|(b, _)| String::from_utf8_lossy(b).into_owned())
                    .unwrap_or_default(),
            ));
        }
        self.mem.send(method, path, body)
    }
}

type Row = (String, String, Option<i64>, String, String);

fn rows(o: &exact::ExactOutcome) -> Vec<Row> {
    o.occurrences
        .iter()
        .map(|x| {
            (
                x.class.as_str().to_owned(),
                x.path.clone(),
                x.line,
                x.matched.clone(),
                x.resolver.clone(),
            )
        })
        .collect()
}

#[test]
fn sqlite_and_server_reads_find_the_same_code_occurrences() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("corpus");
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/endpoints");
    for rel in [
        "src/Content/ContentController.cs",
        "src/Clients/ImageClient.cs",
        "docs/operations.md",
    ] {
        let dest = root.join(rel);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::copy(fixture.join(rel), dest).unwrap();
    }
    let home = Home::new(tmp.path().join("home"));
    let layout = StorageLayout::new(&home);
    let mut cp = ControlPlane::open(&layout, 8).unwrap();
    let canonical = docmonk_core::paths::resolve(&root)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let src = cp
        .add_source(&NewSource {
            canonical_path: canonical.clone(),
            source_type: SourceType::Local,
            enabled: true,
            include_patterns: vec![],
            exclude_patterns: vec![],
        })
        .unwrap()
        .0;
    let cfg = docmonk_config::DocMonkConfig::default();
    run_source(
        &layout,
        &mut cp,
        &src,
        &docmonk_convert::registry_with(&cfg, &docmonk_convert::RegistryOptions::default()),
        &Options::from_config(&cfg),
        &mut NoProgress,
    )
    .unwrap();
    let build = cp.state(&src.id).unwrap().active_build_id.unwrap();
    let mut store =
        ProjectStore::open(&layout, &project_id_for_canonical(&canonical), &src.id, 8).unwrap();
    docmonk_knowledge::build_graph(&mut store, &root, &build, 2, None, &mut |_, _| {}).unwrap();

    // Publish the same graph to the server.
    let engine = Recording::default();
    let server = Arc::new(ServerBackend::with_client(
        Counting::new(Box::new(engine.clone())),
        Engine::OpenSearch,
        "px",
        None,
        BulkLimits::from_config(&cfg.storage.server.bulk),
    ));
    engine.mem.put_doc(
        "px-source-state",
        &src.id,
        json!({"source_id": src.id, "state": "ready", "versions": {}, "active_build_id": build}),
    );
    let staged = store.graph_state_with_files().unwrap();
    let derivation = docmonk_code::process::graph_derivation();
    server
        .publish_graph(&GraphPublishInput {
            source_id: &src.id,
            store: &store,
            local_build: &build,
            base_build_id: &build,
            lease: None,
            generation: &format!("g-{}", staged.generation),
            input_digest: staged.input_digest.as_deref(),
            manifest: Some(GraphManifest {
                derivation: &derivation,
                version: u64::from(docmonk_storage::graph::MANIFEST_VERSION),
                file_keys: &staged.files,
            }),
            phase: None,
        })
        .unwrap();
    let reader = ServerReader::new(server.clone(), &src.id);

    let opts = ExactOptions {
        max_results: 50,
        documents: false,
        filter: Default::default(),
        snippet_tokens: 32,
        max_template_variants: 4,
    };
    for q in [
        "/sb/v1/content/groups/image",
        "/sb/v1/content/groups/image2",
        "/sb/v1/content/groups/42",
    ] {
        let anchors = extract_anchors(q).url_paths;
        let local = exact::find_occurrences(
            &[Corpus {
                store: &store,
                build_id: &build,
            }],
            &anchors,
            &opts,
        )
        .unwrap();
        engine.bodies.lock().unwrap().clear();
        let remote = exact::find_occurrences(
            &[Corpus {
                store: &reader,
                build_id: &build,
            }],
            &anchors,
            &opts,
        )
        .unwrap();
        assert!(!rows(&local).is_empty(), "{q}: fixture has occurrences");
        assert_eq!(rows(&local), rows(&remote), "{q}");
        assert_eq!(local.capabilities[0].exact_occurrences, "indexed");
        assert_eq!(remote.capabilities[0].exact_occurrences, "indexed", "{q}");
        // Exact term lookups only: no wildcard, prefix or query_string.
        let bodies = engine.bodies.lock().unwrap().clone();
        assert!(
            bodies.iter().any(|(_, b)| b.contains("target_symbol")),
            "{bodies:?}"
        );
        for (path, b) in &bodies {
            for scan in ["wildcard", "\"prefix\"", "query_string", "regexp"] {
                assert!(!b.contains(scan), "{q}: {scan} in {path}: {b}");
            }
        }
    }
    // A near miss stays a near miss on both backends.
    let anchors = extract_anchors("/sb/v1/content/groups/image2").url_paths;
    let remote = exact::find_occurrences(
        &[Corpus {
            store: &reader,
            build_id: &build,
        }],
        &anchors,
        &opts,
    )
    .unwrap();
    assert!(rows(&remote)
        .iter()
        .filter(|r| r.0 == "direct_exact_literal")
        .all(|r| r.3.ends_with("image2")));
}
// clean-slate-audit: allow-end
