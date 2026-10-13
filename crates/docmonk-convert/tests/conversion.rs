//! Native document conversion against the expected normalized documents
//! (fixtures/expected/docling.json). Every fixture must convert; headings,
//! tables and text must be preserved.

use std::collections::BTreeSet;
use std::path::Path;

use docmonk_convert::converter::{DoclingConverter, DocumentConverter};
use docmonk_convert::format::detect_format;
use docmonk_convert::normalize::normalize;
use docmonk_documents::model::{NormalizedDocument, UnitKind};
use serde_json::Value;

fn words(doc: &NormalizedDocument) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for u in &doc.units {
        let mut text = u.text.clone();
        for row in u.table_rows.iter().flatten() {
            text.push(' ');
            text.push_str(&row.join(" "));
        }
        out.extend(
            text.split(|c: char| !c.is_alphanumeric())
                .filter(|w| w.chars().count() > 2)
                .map(str::to_lowercase),
        );
    }
    out
}

#[test]
fn every_fixture_converts_and_preserves_content() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let g: Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("fixtures/expected/docling.json")).unwrap(),
    )
    .unwrap();
    let mut report = Vec::new();
    let mut failures = Vec::new();
    let mut exact_count = 0;
    for (name, e) in g.as_object().unwrap() {
        let path = root.join("fixtures/documents").join(name);
        let fmt = detect_format(&path).unwrap();
        assert_eq!(fmt.as_str(), e["format"].as_str().unwrap(), "{name}");
        let json = DoclingConverter::default()
            .convert(&path, fmt)
            .unwrap_or_else(|err| panic!("{name}: {err}"));
        let ours = normalize(&json, false);
        let theirs: NormalizedDocument = serde_json::from_value(e["normalized"].clone()).unwrap();
        let (w_ours, w_theirs) = (words(&ours), words(&theirs));
        let recall = if w_theirs.is_empty() {
            1.0
        } else {
            w_theirs.intersection(&w_ours).count() as f64 / w_theirs.len() as f64
        };
        let tables =
            |d: &NormalizedDocument| d.units.iter().filter(|u| u.kind == UnitKind::Table).count();
        let headings = |d: &NormalizedDocument| {
            d.units
                .iter()
                .filter(|u| u.kind == UnitKind::Heading)
                .count()
        };
        let exact = serde_json::to_value(&ours).unwrap() == e["normalized"];
        exact_count += usize::from(exact);
        report.push(format!(
            "{name:<20} exact {exact:<5} units {:>3}/{:<3} headings {}/{} tables {}/{} word-recall {:.3} title {:?}/{:?}",
            ours.units.len(), theirs.units.len(), headings(&ours), headings(&theirs),
            tables(&ours), tables(&theirs), recall, ours.title, theirs.title
        ));
        // docling.rs is pinned; today every fixture is byte-identical to its
        // expectation after normalization, so any drift is caught here.
        if !exact || recall < 0.95 || tables(&ours) < tables(&theirs) {
            failures.push(name.clone());
        }
    }
    eprintln!(
        "{}\nexact: {exact_count}/{}",
        report.join("\n"),
        report.len()
    );
    assert!(failures.is_empty(), "content lost for {failures:?}");
}

#[test]
fn corrupt_input_fails_cleanly() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/documents");
    let p = root.join("corrupt.docx");
    assert!(DoclingConverter::default()
        .convert(&p, detect_format(&p).unwrap())
        .is_err());
}
