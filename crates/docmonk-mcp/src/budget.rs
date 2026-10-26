//! Per-call result budgets (`mcp.max_result_items`, `mcp.max_result_bytes`,
//! roadmap P8): an answer never floods an agent's context. Lists longer
//! than the item budget are cut, then, while the serialized answer is
//! over the byte budget, the largest list loses its last items and, once
//! no list can shrink, the longest text is shortened. Every cut is
//! reported: `truncated: true` and one `truncation` entry per cut
//! (`path`, `dropped` items or `chars`), so an agent knows to narrow the
//! query instead of trusting a complete-looking answer.

use serde_json::{json, Value};

/// Fields the budget never touches (the result envelope).
const KEEP: &[&str] = &["schema_version", "ok", "error", "truncated", "truncation"];

fn size(v: &Value) -> usize {
    serde_json::to_string(v).map_or(0, |s| s.len())
}

fn cap_lists(v: &mut Value, path: &str, max_items: usize, cuts: &mut Vec<Value>) {
    match v {
        Value::Array(items) => {
            if items.len() > max_items {
                cuts.push(json!({"path": path, "dropped": items.len() - max_items, "reason": "max_result_items"}));
                items.truncate(max_items);
            }
            for (i, item) in items.iter_mut().enumerate() {
                cap_lists(item, &format!("{path}[{i}]"), max_items, cuts);
            }
        }
        Value::Object(o) => {
            for (k, item) in o.iter_mut() {
                if path.is_empty() && KEEP.contains(&k.as_str()) {
                    continue;
                }
                let p = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                cap_lists(item, &p, max_items, cuts);
            }
        }
        _ => {}
    }
}

/// The non-empty list with the largest serialized size, by path.
fn largest_list<'a>(
    v: &'a mut Value,
    path: String,
    top: bool,
) -> Option<(String, usize, &'a mut Vec<Value>)> {
    let mut best: Option<(String, usize, &'a mut Vec<Value>)> = None;
    let mut consider = |c: Option<(String, usize, &'a mut Vec<Value>)>| {
        if let Some(c) = c {
            if best.as_ref().is_none_or(|b| c.1 > b.1) {
                best = Some(c);
            }
        }
    };
    match v {
        Value::Array(items) => {
            let s = items.iter().map(size).sum::<usize>();
            if items.is_empty() {
                return None;
            }
            // A list of lists: shrink the inner ones first only when the
            // outer cannot shrink; the outer itself is the candidate.
            return Some((path, s, items));
        }
        Value::Object(o) => {
            for (k, item) in o.iter_mut() {
                if top && KEEP.contains(&k.as_str()) {
                    continue;
                }
                let p = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                consider(largest_list(item, p, false));
            }
        }
        _ => {}
    }
    best
}

/// The longest string, by path.
fn longest_text<'a>(v: &'a mut Value, path: String, top: bool) -> Option<(String, &'a mut String)> {
    let mut best: Option<(String, &'a mut String)> = None;
    let mut consider = |c: Option<(String, &'a mut String)>| {
        if let Some(c) = c {
            if best.as_ref().is_none_or(|b| c.1.len() > b.1.len()) {
                best = Some(c);
            }
        }
    };
    match v {
        Value::String(s) => return Some((path, s)),
        Value::Array(items) => {
            for (i, item) in items.iter_mut().enumerate() {
                consider(longest_text(item, format!("{path}[{i}]"), false));
            }
        }
        Value::Object(o) => {
            for (k, item) in o.iter_mut() {
                if top && KEEP.contains(&k.as_str()) {
                    continue;
                }
                let p = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                consider(longest_text(item, p, false));
            }
        }
        _ => {}
    }
    best
}

const MARK: &str = " …[truncated]";

/// Applies the budgets to a tool's output object.
pub fn apply(mut out: Value, max_items: usize, max_bytes: usize) -> Value {
    if !out.is_object() {
        return out;
    }
    let mut cuts = Vec::new();
    cap_lists(&mut out, "", max_items.max(1), &mut cuts);
    let mut dropped: std::collections::BTreeMap<String, usize> = Default::default();
    // Bounded: each pass removes an item or halves a text.
    for _ in 0..100_000 {
        let total = size(&out);
        if total <= max_bytes {
            break;
        }
        if let Some((path, list_size, items)) = largest_list(&mut out, String::new(), true) {
            // Drop about as many items as the excess is worth at once.
            let per_item = list_size / items.len().max(1);
            let excess = 1 + total.saturating_sub(max_bytes) / per_item.max(1);
            let n = excess.min(items.len());
            items.truncate(items.len() - n);
            *dropped.entry(path).or_default() += n;
            continue;
        }
        let Some((path, text)) = longest_text(&mut out, String::new(), true) else {
            break;
        };
        if text.chars().count() <= 64 {
            break;
        }
        let keep = text.chars().count() / 2;
        let cut = text.chars().count() - keep;
        *text = text.chars().take(keep).collect::<String>() + MARK;
        cuts.push(json!({"path": path, "chars": cut, "reason": "max_result_bytes"}));
    }
    for (path, n) in dropped {
        cuts.push(json!({"path": path, "dropped": n, "reason": "max_result_bytes"}));
    }
    if !cuts.is_empty() {
        out["truncated"] = json!(true);
        out["truncation"] = Value::Array(cuts);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_answers_pass_untouched() {
        let v = json!({"ok": true, "results": [1, 2, 3]});
        assert_eq!(apply(v.clone(), 10, 10_000), v);
    }

    #[test]
    fn long_lists_are_capped_and_reported() {
        let v = json!({"ok": true, "results": (0..50).collect::<Vec<_>>(), "edges": [1]});
        let out = apply(v, 10, 1_000_000);
        assert_eq!(out["results"].as_array().unwrap().len(), 10);
        assert_eq!(out["edges"], json!([1]));
        assert_eq!(out["truncated"], true);
        assert_eq!(out["truncation"][0]["path"], "results");
        assert_eq!(out["truncation"][0]["dropped"], 40);
    }

    #[test]
    fn large_answers_shrink_under_the_byte_budget() {
        let item = json!({"text": "x".repeat(200)});
        let v = json!({"ok": true, "error": null, "results": vec![item; 100]});
        let out = apply(v, 1000, 4096);
        assert!(size(&out) <= 4096 + 512, "{}", size(&out));
        let kept = out["results"].as_array().unwrap().len();
        assert!(kept > 0 && kept < 100, "{kept}");
        assert_eq!(out["ok"], true);
        let dropped = out["truncation"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["path"] == "results")
            .unwrap()["dropped"]
            .as_u64()
            .unwrap();
        assert_eq!(dropped as usize + kept, 100);
    }

    #[test]
    fn one_huge_text_is_shortened_with_a_marker() {
        let v = json!({"ok": true, "text": "y".repeat(100_000)});
        let out = apply(v, 10, 8192);
        let text = out["text"].as_str().unwrap();
        assert!(text.len() < 8192 && text.ends_with(MARK), "{}", text.len());
        assert_eq!(out["truncation"][0]["path"], "text");
    }
}
