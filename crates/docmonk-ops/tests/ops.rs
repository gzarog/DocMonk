//! Operations end to end (roadmap Q4): backup then restore gives back
//! identical search results, a corrupt archive changes nothing, rebuild
//! reindexes every source and leaves the index alone when it cannot,
//! uninstall never touches user source files, and vector maintenance
//! reports what it did.

use std::path::{Path, PathBuf};

use docmonk_config::DocMonkConfig;
use docmonk_core::paths::Home;
use docmonk_ops::{backup, rebuild, uninstall, vectors};
use serde_json::Value;

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

struct Env {
    d: tempfile::TempDir,
    home: Home,
    root: PathBuf,
    source: String,
}

fn setup() -> Env {
    let d = tempfile::tempdir().unwrap();
    let home = Home::new(d.path().join("home"));
    home.ensure_layout().unwrap();
    let mut cfg = DocMonkConfig::default();
    cfg.updates.enabled = false;
    docmonk_config::write_user_config(&cfg, &home).unwrap();
    let root = d.path().join("project");
    write(
        &root,
        "src/Billing/Invoice.cs",
        "namespace Billing { public class Invoice { public int Total(int a) { return a; } } }\n",
    );
    write(
        &root,
        "docs/invoices.md",
        "# Invoices\n\nInvoices are issued on the first working day of the month.\n",
    );
    let source = docmonk_service::sources::catalog(&home)
        .unwrap()
        .add(root.to_str().unwrap(), vec![], vec![])
        .unwrap()
        .id;
    index(&home);
    Env {
        d,
        home,
        root,
        source,
    }
}

fn index(home: &Home) {
    let s = docmonk_service::indexing::selected_sources(home, None).unwrap();
    docmonk_service::indexing::index_sources(home, &s, "index", |_| {}).unwrap();
}

/// The ranked hits of a lexical search, without timings.
fn search(home: &Home, q: &str) -> Value {
    let cfg = docmonk_service::load(home).unwrap();
    let opened = docmonk_service::query::open_sources(home, None).unwrap();
    docmonk_service::query::lexical_value(&cfg, &opened, q, 20).unwrap()["results"].clone()
}

fn hits(v: &Value) -> usize {
    v.as_array().map_or(0, Vec::len)
}

#[test]
fn backup_then_restore_gives_identical_search_results() {
    let env = setup();
    let before_docs = search(&env.home, "invoices issued");
    let before_code = search(&env.home, "Invoice");
    assert!(hits(&before_docs) > 0 && hits(&before_code) > 0);

    let (archive, manifest) =
        backup::create_backup(&env.home, Some(&env.d.path().join("b.tar.gz"))).unwrap();
    assert!(archive.is_file());
    assert_eq!(manifest["format_version"], backup::MANIFEST_FORMAT_VERSION);
    assert_eq!(manifest["sources"][0]["id"], env.source.as_str());

    // Change the live index after the backup: a new document is indexed.
    write(
        &env.root,
        "docs/credit.md",
        "# Credit notes\n\nCredit notes cancel issued invoices.\n",
    );
    index(&env.home);
    assert!(hits(&search(&env.home, "credit notes")) > 0);

    let report = backup::restore_archive(&env.home, &archive).unwrap();
    assert_eq!(report["projects_restored"].as_array().unwrap().len(), 1);
    assert_eq!(search(&env.home, "invoices issued"), before_docs);
    assert_eq!(search(&env.home, "Invoice"), before_code);
    assert_eq!(hits(&search(&env.home, "credit notes")), 0);
    // The source files themselves are never touched by a restore.
    assert!(env.root.join("docs/credit.md").is_file());
}

#[test]
fn a_corrupt_or_foreign_archive_changes_nothing() {
    let env = setup();
    let before = search(&env.home, "invoices issued");
    let junk = env.d.path().join("junk.tar.gz");
    std::fs::write(&junk, b"not an archive").unwrap();
    assert!(backup::restore_archive(&env.home, &junk).is_err());

    // A well-formed archive without a manifest.
    let empty = env.d.path().join("empty.tar.gz");
    {
        let gz = flate2::write::GzEncoder::new(
            std::fs::File::create(&empty).unwrap(),
            flate2::Compression::default(),
        );
        let mut tar = tar::Builder::new(gz);
        let mut header = tar::Header::new_gnu();
        header.set_size(2);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "readme.txt", &b"hi"[..])
            .unwrap();
        tar.into_inner().unwrap().finish().unwrap();
    }
    let e = backup::restore_archive(&env.home, &empty).unwrap_err();
    assert!(e.message().contains("manifest"), "{}", e.message());
    let e = backup::restore_archive(&env.home, &env.d.path().join("missing.tar.gz")).unwrap_err();
    assert!(e.message().contains("no such archive"), "{}", e.message());
    assert_eq!(search(&env.home, "invoices issued"), before);
}

#[test]
fn rebuild_reindexes_every_source_and_refuses_an_unreachable_fresh_rebuild() {
    let env = setup();
    let before = search(&env.home, "invoices issued");
    let mut seen = 0;
    let rows = rebuild::rebuild_sources(&env.home, None, false, |_| seen += 1).unwrap();
    assert_eq!(seen, 1);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], env.source.as_str());
    assert!(rows[0]["indexed"].as_u64().unwrap() >= 2, "{rows:?}");
    assert!(rows[0].get("error").is_none(), "{rows:?}");
    assert_eq!(search(&env.home, "invoices issued"), before);

    // --fresh with the root gone: refused, the published index stays.
    let moved = env.d.path().join("moved");
    std::fs::rename(&env.root, &moved).unwrap();
    let e = rebuild::rebuild_sources(&env.home, None, true, |_| {}).unwrap_err();
    assert!(e.message().contains("not reachable"), "{}", e.message());
    assert_eq!(search(&env.home, "invoices issued"), before);
    std::fs::rename(&moved, &env.root).unwrap();

    let e = rebuild::rebuild_sources(&env.home, Some("src_nope"), false, |_| {});
    assert!(e.is_err());
}

#[test]
fn uninstall_removes_the_home_and_never_the_source_files() {
    let env = setup();
    let files = ["src/Billing/Invoice.cs", "docs/invoices.md"];
    let before: Vec<Vec<u8>> = files
        .iter()
        .map(|f| std::fs::read(env.root.join(f)).unwrap())
        .collect();
    assert!(uninstall::purge_data(&env.home).unwrap());
    assert!(!env.home.root().exists());
    for (f, bytes) in files.iter().zip(&before) {
        assert_eq!(&std::fs::read(env.root.join(f)).unwrap(), bytes, "{f}");
    }
    // Nothing left to delete.
    assert!(!uninstall::purge_data(&env.home).unwrap());
}

#[test]
fn vector_maintenance_reports_each_source() {
    let env = setup();
    // Semantic search is off by default: no vectors, nothing to rebuild.
    let rows = vectors::rebuild(&env.home, None).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["source_id"], env.source.as_str());
    assert_eq!(rows[0]["vectors"], 0);
    assert!(rows[0]["backend"].is_null());

    let models = std::env::var_os("DOCMONK_MODELS_DIR")
        .map(PathBuf::from)
        .filter(|d| d.join("all-minilm-l6-v2").is_dir());
    match models {
        Some(_) => {
            // Missing vectors are computed, then the ANN index is built.
            let done = vectors::backfill(&env.home, None).unwrap();
            assert!(done[0]["embedded"].as_u64().unwrap() > 0, "{done:?}");
            let rows = vectors::rebuild(&env.home, Some(&env.source)).unwrap();
            assert_eq!(rows[0]["backend"], "hnsw");
            assert!(rows[0]["vectors"].as_u64().unwrap() > 0, "{rows:?}");
            // Nothing left to backfill.
            let again = vectors::backfill(&env.home, None).unwrap();
            assert_eq!(again[0]["embedded"], 0, "{again:?}");
        }
        None => {
            if std::env::var("DOCMONK_REQUIRE_MODELS").as_deref() == Ok("1") {
                panic!("DOCMONK_REQUIRE_MODELS=1 but DOCMONK_MODELS_DIR has no embedding model");
            }
            let e = vectors::backfill(&env.home, None).unwrap_err();
            assert!(e.message().contains("not available"), "{}", e.message());
        }
    }
}
