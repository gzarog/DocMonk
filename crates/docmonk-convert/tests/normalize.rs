//! The normalizer turns each converter document in
//! fixtures/expected/docling.json into exactly its expected normalized form.

use std::path::Path;

use docmonk_convert::normalize::normalize;
use serde_json::Value;

#[test]
fn normalizer_output_is_as_expected() {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/expected/docling.json");
    let g: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    let mut failures = Vec::new();
    for (name, e) in g.as_object().unwrap() {
        let got = normalize(&e["docling"], e["format"] == "pdf");
        let got = serde_json::to_value(&got).unwrap();
        if got != e["normalized"] {
            failures.push(format!(
                "{name}:\n got      {got}\n expected {}",
                e["normalized"]
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
