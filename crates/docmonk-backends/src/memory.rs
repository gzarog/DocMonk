//! A deterministic in-memory search engine for tests.
//!
//! It answers the subset of the OpenSearch/Elasticsearch HTTP API that
//! DocMonk's writers and readers use (document get/put with optimistic
//! concurrency, `_bulk`, `_search` / `_msearch` / `_count` with `bool`,
//! `term(s)`, `range`, `exists` queries, `sort` and `search_after`, and
//! `_update_by_query` / `_delete_by_query`), evaluating the queries exactly
//! as an engine would. Lexical scoring, aggregations and kNN are not
//! modelled. It counts what every request touched, so tests can assert
//! that an operation rewrote no unchanged record.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::error::Result;
use crate::transport::{Method, Response, Transport};

/// What the engine was asked to do, cumulatively.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Touched {
    /// Documents indexed by `_bulk` (new or overwritten).
    pub indexed: u64,
    /// Documents changed by `_update_by_query`.
    pub updated: u64,
    /// Documents removed by `_delete_by_query` or `_bulk` deletes.
    pub deleted: u64,
}

#[derive(Default)]
struct Store {
    /// `index -> id -> (seq_no, doc)`.
    docs: BTreeMap<String, BTreeMap<String, (i64, Value)>>,
    seq: i64,
    touched: Touched,
    /// Fail the next `_bulk` (failure injection).
    fail_bulk: u32,
    /// Fail the next state write to this path fragment.
    fail_put: Option<String>,
}

/// The engine; clones share state.
#[derive(Clone, Default)]
pub struct Memory(Arc<Mutex<Store>>);

fn reply(status: u16, v: Value) -> Result<Response> {
    Ok(Response {
        status,
        body: serde_json::to_vec(&v).unwrap_or_default(),
    })
}

fn field<'a>(doc: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = doc;
    for p in path.split('.') {
        cur = cur.get(p)?;
    }
    Some(cur).filter(|v| !v.is_null())
}

fn values(doc: &Value, path: &str) -> Vec<Value> {
    match field(doc, path) {
        Some(Value::Array(a)) => a.clone(),
        Some(v) => vec![v.clone()],
        None => Vec::new(),
    }
}

fn same(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

fn cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a.as_i64(), b.as_i64()) {
        (Some(x), Some(y)) => x.cmp(&y),
        _ => match (a.as_f64(), b.as_f64()) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
            _ => a
                .as_str()
                .unwrap_or_default()
                .cmp(b.as_str().unwrap_or_default()),
        },
    }
}

/// `*`-only wildcard matching (no `?`, no escapes).
fn glob(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !text.starts_with(first) || text.len() < first.len() + last.len() {
        return false;
    }
    let mut rest = &text[first.len()..text.len() - last.len()];
    if !text.ends_with(last) {
        return false;
    }
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

/// Whether `doc` matches `q`.
pub fn matches(doc: &Value, q: &Value) -> bool {
    let Some((kind, body)) = q.as_object().and_then(|o| o.iter().next()) else {
        return true;
    };
    match kind.as_str() {
        "match_all" => true,
        "match_none" => false,
        "term" => body.as_object().is_some_and(|o| {
            o.iter().all(|(f, v)| {
                let v = v.get("value").unwrap_or(v);
                values(doc, f).iter().any(|x| same(x, v))
            })
        }),
        "terms" => body.as_object().is_some_and(|o| {
            o.iter().all(|(f, want)| {
                let want = want.as_array().cloned().unwrap_or_default();
                values(doc, f)
                    .iter()
                    .any(|x| want.iter().any(|w| same(x, w)))
            })
        }),
        "range" => body.as_object().is_some_and(|o| {
            o.iter().all(|(f, r)| {
                values(doc, f).iter().any(|x| {
                    let ok = |op: &str, pred: fn(std::cmp::Ordering) -> bool| {
                        r.get(op).is_none_or(|b| pred(cmp(x, b)))
                    };
                    ok("gt", |o| o.is_gt())
                        && ok("gte", |o| o.is_ge())
                        && ok("lt", |o| o.is_lt())
                        && ok("lte", |o| o.is_le())
                })
            })
        }),
        "prefix" => body.as_object().is_some_and(|o| {
            o.iter().all(|(f, v)| {
                let v = v.get("value").unwrap_or(v).as_str().unwrap_or_default();
                values(doc, f)
                    .iter()
                    .any(|x| x.as_str().is_some_and(|x| x.starts_with(v)))
            })
        }),
        "wildcard" => body.as_object().is_some_and(|o| {
            o.iter().all(|(f, v)| {
                let v = v.get("value").unwrap_or(v).as_str().unwrap_or_default();
                values(doc, f)
                    .iter()
                    .any(|x| x.as_str().is_some_and(|x| glob(v, x)))
            })
        }),
        "exists" => body["field"]
            .as_str()
            .is_some_and(|f| field(doc, f).is_some()),
        "ids" => body["values"]
            .as_array()
            .is_some_and(|ids| ids.iter().any(|i| Some(i) == doc.get("_id"))),
        "bool" => {
            let list = |k: &str| body[k].as_array().cloned().unwrap_or_default();
            let (must, filter, should, must_not) = (
                list("must"),
                list("filter"),
                list("should"),
                list("must_not"),
            );
            let min = body["minimum_should_match"].as_u64().unwrap_or(
                if must.is_empty() && filter.is_empty() && !should.is_empty() {
                    1
                } else {
                    0
                },
            ) as usize;
            must.iter().chain(&filter).all(|c| matches(doc, c))
                && !must_not.iter().any(|c| matches(doc, c))
                && should.iter().filter(|c| matches(doc, c)).count() >= min
        }
        // Lexical queries are not scored here: every document matches.
        _ => true,
    }
}

fn sort_keys(sort: &Value) -> Vec<(String, bool)> {
    sort.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| match s {
            Value::String(f) => Some((f.clone(), false)),
            Value::Object(o) => o.iter().next().map(|(f, d)| {
                let desc = d.as_str().or_else(|| d["order"].as_str()) == Some("desc");
                (f.clone(), desc)
            }),
            _ => None,
        })
        .filter(|(f, _)| f != "_score")
        .collect()
}

impl Memory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn touched(&self) -> Touched {
        self.lock().touched.clone()
    }

    /// Fails the next `n` bulk requests.
    pub fn fail_next_bulk(&self, n: u32) {
        self.lock().fail_bulk = n;
    }

    /// Fails the next document write whose path contains `fragment`.
    pub fn fail_next_put(&self, fragment: &str) {
        self.lock().fail_put = Some(fragment.to_owned());
    }

    /// Every stored document of `index` (with `_id`).
    pub fn docs(&self, index: &str) -> Vec<Value> {
        self.lock()
            .docs
            .get(index)
            .into_iter()
            .flat_map(|m| m.iter())
            .map(|(id, (_, d))| {
                let mut d = d.clone();
                d["_id"] = json!(id);
                d
            })
            .collect()
    }

    /// Replaces one stored document (tests that simulate another writer).
    pub fn put_doc(&self, index: &str, id: &str, doc: Value) {
        let mut s = self.lock();
        s.seq += 1;
        let seq = s.seq;
        s.docs
            .entry(index.to_owned())
            .or_default()
            .insert(id.to_owned(), (seq, doc));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Store> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn search(s: &Store, indexes: &str, body: &Value) -> Vec<(String, String, Value)> {
        let q = body
            .get("query")
            .cloned()
            .unwrap_or(json!({ "match_all": {} }));
        let mut hits: Vec<(String, String, Value)> = Vec::new();
        for index in indexes.split(',') {
            for (id, (_, d)) in s.docs.get(index).into_iter().flatten() {
                let mut with_id = d.clone();
                with_id["_id"] = json!(id);
                if matches(&with_id, &q) {
                    hits.push((index.to_owned(), id.clone(), d.clone()));
                }
            }
        }
        let keys = sort_keys(&body["sort"]);
        let key = |d: &Value| -> Vec<Value> {
            keys.iter()
                .map(|(f, _)| field(d, f).cloned().unwrap_or(Value::Null))
                .collect()
        };
        hits.sort_by(|a, b| {
            for (i, (_, desc)) in keys.iter().enumerate() {
                let o = cmp(&key(&a.2)[i], &key(&b.2)[i]);
                let o = if *desc { o.reverse() } else { o };
                if o.is_ne() {
                    return o;
                }
            }
            (&a.0, &a.1).cmp(&(&b.0, &b.1))
        });
        if let Some(after) = body["search_after"].as_array() {
            hits.retain(|h| {
                let k = key(&h.2);
                for (i, (_, desc)) in keys.iter().enumerate() {
                    let o = cmp(&k[i], after.get(i).unwrap_or(&Value::Null));
                    let o = if *desc { o.reverse() } else { o };
                    if o.is_ne() {
                        return o.is_gt();
                    }
                }
                false
            });
        }
        let size = body["size"].as_u64().unwrap_or(10) as usize;
        hits.truncate(size);
        hits
    }

    fn hits_json(hits: Vec<(String, String, Value)>, keys: &[(String, bool)]) -> Value {
        let hits: Vec<Value> = hits
            .into_iter()
            .map(|(index, id, d)| {
                let sort: Vec<Value> = keys
                    .iter()
                    .map(|(f, _)| field(&d, f).cloned().unwrap_or(Value::Null))
                    .collect();
                json!({ "_index": index, "_id": id, "_score": 1.0, "_source": d, "sort": sort })
            })
            .collect();
        json!({ "hits": { "total": { "value": hits.len() }, "hits": hits } })
    }
}

impl Transport for Memory {
    fn send(&self, method: Method, path: &str, body: Option<(&[u8], &str)>) -> Result<Response> {
        let mut s = self.lock();
        let (base, query) = path.split_once('?').unwrap_or((path, ""));
        let body_json = || -> Value {
            body.and_then(|(b, _)| serde_json::from_slice(b).ok())
                .unwrap_or(Value::Null)
        };
        let parts: Vec<&str> = base.trim_start_matches('/').split('/').collect();
        match (method, parts.as_slice()) {
            (Method::Post, ["_bulk"]) => {
                if s.fail_bulk > 0 {
                    s.fail_bulk -= 1;
                    return reply(500, json!({ "error": "injected bulk failure" }));
                }
                let text = body.map(|(b, _)| String::from_utf8_lossy(b).into_owned());
                let lines: Vec<Value> = text
                    .unwrap_or_default()
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .filter_map(|l| serde_json::from_str(l).ok())
                    .collect();
                let mut items = Vec::new();
                let mut i = 0;
                while i < lines.len() {
                    let action = &lines[i];
                    if let Some(a) = action.get("index") {
                        let (index, id) = (
                            a["_index"].as_str().unwrap_or_default(),
                            a["_id"].as_str().unwrap_or_default(),
                        );
                        s.seq += 1;
                        let seq = s.seq;
                        let doc = lines.get(i + 1).cloned().unwrap_or(Value::Null);
                        s.docs
                            .entry(index.to_owned())
                            .or_default()
                            .insert(id.to_owned(), (seq, doc));
                        s.touched.indexed += 1;
                        items.push(json!({ "index": { "status": 201 } }));
                        i += 2;
                    } else if let Some(a) = action.get("delete") {
                        let (index, id) = (
                            a["_index"].as_str().unwrap_or_default(),
                            a["_id"].as_str().unwrap_or_default(),
                        );
                        if s.docs.get_mut(index).and_then(|m| m.remove(id)).is_some() {
                            s.touched.deleted += 1;
                        }
                        items.push(json!({ "delete": { "status": 200 } }));
                        i += 1;
                    } else {
                        i += 1;
                    }
                }
                reply(200, json!({ "errors": false, "items": items }))
            }
            (Method::Post, ["_msearch"]) => {
                let text = body.map(|(b, _)| String::from_utf8_lossy(b).into_owned());
                let lines: Vec<Value> = text
                    .unwrap_or_default()
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .filter_map(|l| serde_json::from_str(l).ok())
                    .collect();
                let responses: Vec<Value> = lines
                    .chunks(2)
                    .map(|pair| {
                        let index = pair[0]["index"].as_str().unwrap_or_default();
                        let b = pair.get(1).cloned().unwrap_or(Value::Null);
                        let keys = sort_keys(&b["sort"]);
                        let mut r = Self::hits_json(Self::search(&s, index, &b), &keys);
                        r["status"] = json!(200);
                        r
                    })
                    .collect();
                reply(200, json!({ "responses": responses }))
            }
            (_, [_, "_refresh"]) | (_, ["_refresh"]) => reply(200, json!({})),
            (Method::Post | Method::Get, [indexes, "_search"]) => {
                let b = body_json();
                let keys = sort_keys(&b["sort"]);
                let hits = Self::search(&s, indexes, &b);
                reply(200, Self::hits_json(hits, &keys))
            }
            (Method::Post | Method::Get, [indexes, "_count"]) => {
                let mut b = body_json();
                b["size"] = json!(u64::MAX);
                let n = Self::search(&s, indexes, &b).len();
                reply(200, json!({ "count": n }))
            }
            (Method::Post, [indexes, "_update_by_query"]) => {
                let b = body_json();
                let mut all = b.clone();
                all["size"] = json!(u64::MAX);
                let hits = Self::search(&s, indexes, &all);
                let v = b["script"]["params"]["v"].clone();
                let src = b["script"]["source"].as_str().unwrap_or_default();
                assert_eq!(
                    src,
                    crate::generations::SET_VALID_TO,
                    "the memory engine only runs the valid_to script"
                );
                let n = hits.len() as u64;
                for (index, id, _) in hits {
                    s.seq += 1;
                    let seq = s.seq;
                    if let Some(e) = s.docs.get_mut(&index).and_then(|m| m.get_mut(&id)) {
                        e.1["valid_to"] = v.clone();
                        e.0 = seq;
                    }
                }
                s.touched.updated += n;
                reply(200, json!({ "updated": n, "total": n, "failures": [] }))
            }
            (Method::Post, [indexes, "_delete_by_query"]) => {
                let mut b = body_json();
                b["size"] = json!(u64::MAX);
                let hits = Self::search(&s, indexes, &b);
                let n = hits.len() as u64;
                for (index, id, _) in hits {
                    if let Some(m) = s.docs.get_mut(&index) {
                        m.remove(&id);
                    }
                }
                s.touched.deleted += n;
                reply(200, json!({ "deleted": n, "total": n, "failures": [] }))
            }
            (Method::Get, [index, "_doc", id]) => {
                let id = percent_decode(id);
                match s.docs.get(*index).and_then(|m| m.get(&id)) {
                    Some((seq, d)) => reply(
                        200,
                        json!({ "_id": id, "_source": d, "_seq_no": seq, "_primary_term": 1, "found": true }),
                    ),
                    None => reply(404, json!({ "found": false })),
                }
            }
            (Method::Put | Method::Post, [index, op @ ("_doc" | "_create"), id]) => {
                if s.fail_put.as_deref().is_some_and(|f| path.contains(f)) {
                    s.fail_put = None;
                    return reply(503, json!({ "error": "injected write failure" }));
                }
                let id = percent_decode(id);
                let current = s.docs.get(*index).and_then(|m| m.get(&id)).map(|e| e.0);
                if *op == "_create" && current.is_some() {
                    return reply(409, json!({ "error": "version_conflict" }));
                }
                if let Some(want) = query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("if_seq_no="))
                    .and_then(|v| v.parse::<i64>().ok())
                {
                    if current != Some(want) {
                        return reply(409, json!({ "error": "version_conflict" }));
                    }
                }
                s.seq += 1;
                let seq = s.seq;
                s.docs
                    .entry((*index).to_owned())
                    .or_default()
                    .insert(id, (seq, body_json()));
                reply(200, json!({ "result": "updated", "_seq_no": seq }))
            }
            (Method::Delete, [index, "_doc", id]) => {
                let id = percent_decode(id);
                match s.docs.get_mut(*index).and_then(|m| m.remove(&id)) {
                    Some(_) => reply(200, json!({ "result": "deleted" })),
                    None => reply(404, json!({ "result": "not_found" })),
                }
            }
            _ => reply(
                400,
                json!({ "error": format!("memory engine: unsupported {method:?} {path}") }),
            ),
        }
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 3 <= b.len() {
            if let Some(v) = std::str::from_utf8(&b[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_evaluate_like_an_engine() {
        let d = json!({"source_id": "s", "valid_from": 2, "valid_to": 5, "tags": ["a", "b"]});
        assert!(matches(&d, &json!({"term": {"tags": "b"}})));
        assert!(matches(&d, &json!({"range": {"valid_from": {"lte": 2}}})));
        assert!(!matches(&d, &json!({"range": {"valid_to": {"gt": 5}}})));
        assert!(matches(&d, &crate::generations::visible_at(3, &[])));
        assert!(!matches(&d, &crate::generations::visible_at(5, &[])));
        assert!(matches(&d, &crate::generations::visible_at(5, &[5])));
        assert!(!matches(&d, &crate::generations::visible_at(3, &[2])));
        assert!(matches(
            &d,
            &json!({"bool": {"should": [{"term": {"source_id": "x"}}, {"term": {"source_id": "s"}}]}})
        ));
    }
}
