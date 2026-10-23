//! Tokenizer identity, counts and splits against
//! fixtures/expected/documents.json.

use std::path::Path;

use docmonk_documents::tokenization::{split_by_token_budget, split_sentences};
use docmonk_documents::tokenizer::{self, model_tokenizer};
use serde_json::Value;

fn golden() -> Value {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/expected/documents.json");
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

#[test]
fn identity_is_as_expected() {
    let g = golden();
    let id = &g["identity"];
    assert_eq!(id["model_id"], tokenizer::EMBEDDING_MODEL_ID);
    assert_eq!(id["revision"], tokenizer::TOKENIZER_REVISION);
    assert_eq!(id["fingerprint"], tokenizer::tokenizer_fingerprint());
    assert_eq!(id["preprocessing"], tokenizer::preprocessing_fingerprint());
    assert_eq!(id["max_sequence_tokens"], tokenizer::MAX_SEQUENCE_TOKENS);
    for (name, digest) in tokenizer::TOKENIZER_ASSET_MANIFEST {
        assert_eq!(id["manifest"][name], *digest);
    }
}

#[test]
fn counts_and_splits_are_as_expected() {
    let g = golden();
    let tok = model_tokenizer().unwrap();
    let mut failures = Vec::new();
    for e in g["tokens"].as_array().unwrap() {
        let text = e["text"].as_str().unwrap();
        if tok.count(text, true) != e["with_special"].as_i64().unwrap()
            || tok.count(text, false) != e["body"].as_i64().unwrap()
        {
            failures.push(format!("count {text:?}"));
        }
        if let Some(splits) = e["split"].as_object() {
            for (b, want) in splits {
                let got = tok.split(text, b.parse().unwrap());
                if serde_json::to_value(&got).unwrap() != *want {
                    failures.push(format!("split {text:?} {b}: {got:?} vs {want}"));
                }
            }
        }
        if let Some(splits) = e["budget_split"].as_object() {
            for (b, want) in splits {
                let got = split_by_token_budget(text, b.parse().unwrap());
                if serde_json::to_value(&got).unwrap() != *want {
                    failures.push(format!("budget_split {text:?} {b}: {got:?} vs {want}"));
                }
            }
        }
    }
    for e in g["sentences"].as_array().unwrap() {
        let got = split_sentences(e["text"].as_str().unwrap());
        if serde_json::to_value(&got).unwrap() != e["sentences"] {
            failures.push(format!("sentences {:?}: {got:?}", e["text"]));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn sentences_end_at_cjk_and_arabic_full_stops() {
    assert_eq!(
        split_sentences("経費精算は30日以内です。領収書を添付してください。次へ"),
        [
            "経費精算は30日以内です。",
            "領収書を添付してください。",
            "次へ"
        ]
    );
    assert_eq!(
        split_sentences("备份保留三十天！ 恢复演练每季度一次？是的"),
        ["备份保留三十天！", "恢复演练每季度一次？", "是的"]
    );
    assert_eq!(
        split_sentences("هل يجب تفعيل المصادقة؟ نعم، دائما."),
        ["هل يجب تفعيل المصادقة؟", "نعم، دائما."]
    );
    // Latin text is unchanged: a full stop needs the following space.
    assert_eq!(
        split_sentences("Version 2.5 is out. Upgrade now!"),
        ["Version 2.5 is out.", "Upgrade now!"]
    );
}

#[test]
fn budget_splits_never_insert_spaces_into_japanese() {
    let text = "経費精算は三十日以内に提出してください。".repeat(40);
    let pieces = split_by_token_budget(&text, 64);
    assert!(pieces.len() > 1);
    assert!(pieces.iter().all(|p| !p.contains(' ')), "{pieces:?}");
    assert_eq!(pieces.concat(), text);
}
