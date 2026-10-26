//! Extraction and relationship building over fixtures/code, against the
//! expected entities and relationships in fixtures/expected/code.json.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use docmonk_code::process::{
    build_relationships, candidates, entity_rows, prepare_source, PreparedCode,
};
use docmonk_code::resolve::{bare_name, resolve_reference};
use docmonk_storage::knowledge::EntityRow;
use serde_json::{json, Value};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let rel = p
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push(rel);
            }
        }
    }
    out.sort();
    out
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

fn canon(mut v: Vec<Value>) -> Vec<Value> {
    v.sort_by_key(sort_key);
    v
}

#[test]
fn extraction_and_relationships_are_as_expected() {
    let root = repo_root().join("fixtures/code");
    let golden: Value = serde_json::from_str(
        &std::fs::read_to_string(repo_root().join("fixtures/expected/code.json")).unwrap(),
    )
    .unwrap();
    let golden = &golden["files"];
    // `DOCMONK_BLESS=1` re-records every file's relationships (a
    // deliberate derivation change); entities are never re-recorded.
    let bless = std::env::var("DOCMONK_BLESS").as_deref() == Ok("1");
    let mut blessed: BTreeMap<String, Vec<Value>> = BTreeMap::new();

    let mut prepared = BTreeMap::new();
    for rel in files(&root) {
        // Normalize CRLF checkouts so byte columns match the expectation.
        let raw = std::fs::read(root.join(&rel)).unwrap();
        let source = String::from_utf8_lossy(&raw)
            .replace("\r\n", "\n")
            .into_bytes();
        prepared.insert(rel.clone(), prepare_source(&source, &rel));
    }
    assert_eq!(
        prepared.keys().collect::<Vec<_>>(),
        golden.as_object().unwrap().keys().collect::<Vec<_>>()
    );

    // Entities keyed as in the expected file: "<rel>#<local_id>".
    let mut entities: BTreeMap<String, (Vec<String>, Vec<EntityRow>)> = BTreeMap::new();
    for (rel, p) in &prepared {
        if let Ok(PreparedCode::Parsed {
            language,
            extraction,
        }) = p
        {
            let ids: Vec<String> = (0..extraction.entities.len())
                .map(|i| format!("{rel}#{i}"))
                .collect();
            let rows = entity_rows(rel, language, extraction, &ids);
            entities.insert(rel.clone(), (ids, rows));
        }
    }
    let all: Vec<EntityRow> = entities.values().flat_map(|(_, r)| r.clone()).collect();

    let mut failures = Vec::new();
    for (rel, p) in &prepared {
        let g = &golden[rel];
        match p {
            Err(_) => {
                if g["error"] != "parse_error" {
                    failures.push(format!("{rel}: unexpected parse error"));
                }
                continue;
            }
            Ok(PreparedCode::Unsupported) => {
                if !g["language"].is_null() {
                    failures.push(format!("{rel}: expected language {}", g["language"]));
                }
                continue;
            }
            Ok(PreparedCode::Parsed {
                language,
                extraction,
            }) => {
                if g["language"] != *language {
                    failures.push(format!("{rel}: language {language} vs {}", g["language"]));
                }
                let (ids, rows) = &entities[rel];
                let got_entities: Vec<Value> = rows
                    .iter()
                    .map(|e| {
                        json!({
                            "key": e.id, "kind": e.kind, "name": e.name,
                            "qualified_name": e.qualified_name, "parent": e.parent_id,
                            "signature": e.signature,
                            "range": [e.start_line, e.start_col, e.end_line, e.end_col],
                        })
                    })
                    .collect();
                if got_entities != g["entities"].as_array().cloned().unwrap_or_default() {
                    failures.push(format!(
                        "{rel}: entities differ\n got:      {}\n expected: {}",
                        serde_json::to_string(&got_entities).unwrap(),
                        serde_json::to_string(&g["entities"]).unwrap()
                    ));
                    continue;
                }
                let same_file = candidates(rows);
                let others: Vec<EntityRow> =
                    all.iter().filter(|e| &e.file_id != rel).cloned().collect();
                let other_c = candidates(&others);
                let rels = build_relationships(rel, rel, language, extraction, ids, &mut |text| {
                    resolve_reference(
                        text,
                        &same_file,
                        |t| {
                            other_c
                                .iter()
                                .filter(|c| c.qualified_name == t)
                                .copied()
                                .collect()
                        },
                        |n| {
                            other_c
                                .iter()
                                .filter(|c| c.name == bare_name(n))
                                .copied()
                                .collect()
                        },
                    )
                });
                let got: Vec<Value> = canon(
                    rels.iter()
                        .map(|r| {
                            json!({
                                "type": r.relationship_type, "source": r.source_entity_id,
                                "target": r.target_entity_id, "symbol": r.target_symbol,
                                "resolver": r.resolver, "confidence": r.confidence,
                                "location": r.source_location, "evidence": r.evidence,
                            })
                        })
                        .collect(),
                );
                let want = canon(g["relationships"].as_array().cloned().unwrap_or_default());
                if bless {
                    blessed.insert(rel.clone(), got);
                    continue;
                }
                if got != want {
                    let only_rust: Vec<_> = got.iter().filter(|x| !want.contains(x)).collect();
                    let only_py: Vec<_> = want.iter().filter(|x| !got.contains(x)).collect();
                    failures.push(format!(
                        "{rel}: relationships differ\n only got:      {only_rust:?}\n only expected: {only_py:?}"
                    ));
                }
            }
        }
    }
    if bless {
        let path = repo_root().join("fixtures/expected/code.json");
        let mut all: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for (rel, rels) in blessed {
            all["files"][&rel]["relationships"] = Value::Array(rels);
        }
        let mut out = Vec::new();
        let fmt = serde_json::ser::PrettyFormatter::with_indent(b" ");
        serde::Serialize::serialize(
            &all,
            &mut serde_json::Serializer::with_formatter(&mut out, fmt),
        )
        .unwrap();
        out.push(b'\n');
        std::fs::write(path, out).unwrap();
        return;
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
