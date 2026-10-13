//! Secret and URL-credential redaction against
//! fixtures/expected/core_redaction.json.

use docmonk_telemetry::redact::{redact_url, redact_urls_in_text_with, url_has_userinfo};
use serde_json::Value;

fn golden() -> Value {
    let path = format!(
        "{}/../../fixtures/expected/core_redaction.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn text_redaction_is_as_expected() {
    let g = golden();
    let env = g["env"].clone();
    let lookup = move |k: &str| env.get(k).and_then(|v| v.as_str()).map(str::to_owned);
    for c in g["texts"].as_array().unwrap() {
        let input = c["input"].as_str().unwrap();
        assert_eq!(
            redact_urls_in_text_with(input, &lookup),
            c["output"].as_str().unwrap(),
            "{input:?}"
        );
    }
}

#[test]
fn url_redaction_is_as_expected() {
    for c in golden()["urls"].as_array().unwrap() {
        let url = c["url"].as_str().unwrap();
        assert_eq!(redact_url(url), c["redacted"].as_str().unwrap(), "{url:?}");
        assert_eq!(
            url_has_userinfo(url),
            c["has_userinfo"].as_bool().unwrap(),
            "{url:?}"
        );
    }
}
