//! Rendering JSON values inside messages and prompts.

use serde_json::Value;

/// A value as plain text: strings as-is, `null` as empty, anything else as
/// compact JSON.
pub fn plain(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// A value quoted for an error message: compact JSON (strings in double
/// quotes).
pub fn quoted(v: &Value) -> String {
    v.to_string()
}

/// A response body for an error message: the body as sent (trimmed) when
/// it is JSON, `None` otherwise. Re-serializing would make the key order
/// depend on serde_json's enabled features.
pub fn json_body(text: &str) -> Option<String> {
    let trimmed = text.trim();
    serde_json::from_str::<Value>(trimmed)
        .is_ok()
        .then(|| trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rendering() {
        assert_eq!(plain(&json!("a b")), "a b");
        assert_eq!(plain(&json!(null)), "");
        assert_eq!(plain(&json!({"k": [1, true]})), r#"{"k":[1,true]}"#);
        assert_eq!(quoted(&json!("x")), r#""x""#);
        assert_eq!(json_body(" {\"e\": 1} ").as_deref(), Some(r#"{"e": 1}"#));
        assert_eq!(json_body("<html>"), None);
    }
}
