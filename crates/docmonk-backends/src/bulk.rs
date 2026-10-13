//! Bounded, adaptive bulk writes.
//!
//! * A batch never exceeds `max_actions` actions or `max_bytes` payload
//!   bytes (a single oversize action is sent alone).
//! * HTTP 413/429 for a whole request halves the target batch size and
//!   retries; success grows it back toward `max_actions`.
//! * Item-level 429/503 (`es_rejected_execution_exception`, …) are retried
//!   with exponential backoff up to `max_retries`; anything else is a
//!   terminal failure reported once the writer finishes.
//! * Memory is bounded by one batch: actions are serialized as they are
//!   pushed and flushed as soon as a limit would be crossed.

use std::time::Duration;

use serde_json::Value;

use crate::error::{BackendError, Result};
use crate::transport::{Client, Method};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BulkLimits {
    pub max_actions: usize,
    pub max_bytes: usize,
    pub max_retries: u32,
    pub base_backoff: Duration,
}

impl BulkLimits {
    pub fn from_config(bulk: &docmonk_config::model::BulkConfig) -> Self {
        Self {
            max_actions: bulk.max_actions.max(1) as usize,
            max_bytes: bulk.max_bytes.max(1) as usize,
            max_retries: bulk.max_retries.max(0) as u32,
            base_backoff: Duration::from_millis(200),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Action {
    Index {
        index: String,
        id: String,
        doc: Value,
    },
    Delete {
        index: String,
        id: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct BulkReport {
    pub actions: usize,
    pub requests: usize,
    pub retried_items: usize,
    pub failed: usize,
    pub first_error: Option<String>,
}

struct Encoded {
    bytes: Vec<u8>,
}

fn encode(action: &Action) -> Result<Encoded> {
    let mut bytes = Vec::new();
    let push_line = |bytes: &mut Vec<u8>, v: &Value| -> Result<()> {
        serde_json::to_writer(&mut *bytes, v).map_err(|e| BackendError::Invalid(e.to_string()))?;
        bytes.push(b'\n');
        Ok(())
    };
    match action {
        Action::Index { index, id, doc } => {
            push_line(
                &mut bytes,
                &serde_json::json!({ "index": { "_index": index, "_id": id } }),
            )?;
            push_line(&mut bytes, doc)?;
        }
        Action::Delete { index, id } => {
            push_line(
                &mut bytes,
                &serde_json::json!({ "delete": { "_index": index, "_id": id } }),
            )?;
        }
    }
    Ok(Encoded { bytes })
}

fn retryable_status(status: u64) -> bool {
    status == 429 || status == 503
}

pub struct BulkWriter<'a> {
    client: &'a Client,
    limits: BulkLimits,
    target: usize,
    pending: Vec<Encoded>,
    pending_bytes: usize,
    report: BulkReport,
}

impl<'a> BulkWriter<'a> {
    pub fn new(client: &'a Client, limits: BulkLimits) -> Self {
        Self {
            client,
            limits,
            target: limits.max_actions,
            pending: Vec::new(),
            pending_bytes: 0,
            report: BulkReport::default(),
        }
    }

    pub fn push(&mut self, action: &Action) -> Result<()> {
        let enc = encode(action)?;
        let would_exceed = !self.pending.is_empty()
            && (self.pending.len() + 1 > self.target
                || self.pending_bytes + enc.bytes.len() > self.limits.max_bytes);
        if would_exceed {
            self.flush()?;
        }
        self.pending_bytes += enc.bytes.len();
        self.pending.push(enc);
        self.report.actions += 1;
        if self.pending.len() >= self.target {
            self.flush()?;
        }
        Ok(())
    }

    /// Sends what is pending. Terminal item failures are accumulated.
    pub fn flush(&mut self) -> Result<()> {
        let mut batch: Vec<Encoded> = std::mem::take(&mut self.pending);
        self.pending_bytes = 0;
        let mut attempt = 0u32;
        while !batch.is_empty() {
            let take = batch.len().min(self.target.max(1));
            let rest = batch.split_off(take);
            let body: Vec<u8> = batch.iter().flat_map(|e| e.bytes.iter().copied()).collect();
            self.report.requests += 1;
            self.client.record_bulk(batch.len(), body.len());
            let resp = self.client.send(
                Method::Post,
                "/_bulk",
                Some((&body, "application/x-ndjson")),
            )?;
            if resp.status == 413 || resp.status == 429 {
                // Whole request rejected: shrink and retry the same actions.
                if attempt >= self.limits.max_retries && self.target == 1 {
                    return Err(BackendError::Http {
                        method: "POST".into(),
                        path: "/_bulk".into(),
                        status: resp.status,
                        reason: "bulk request rejected after retries".into(),
                    });
                }
                self.target = (self.target / 2).max(1);
                attempt += 1;
                std::thread::sleep(self.backoff(attempt));
                batch.extend(rest);
                continue;
            }
            if resp.status >= 300 {
                return Err(BackendError::Http {
                    method: "POST".into(),
                    path: "/_bulk".into(),
                    status: resp.status,
                    reason: String::from_utf8_lossy(&resp.body)
                        .chars()
                        .take(300)
                        .collect(),
                });
            }
            let json = resp.json()?;
            let mut retry = Vec::new();
            if json.get("errors").and_then(Value::as_bool) == Some(true) {
                let items = json
                    .get("items")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for (item, enc) in items.iter().zip(batch) {
                    let result = item
                        .as_object()
                        .and_then(|o| o.values().next())
                        .cloned()
                        .unwrap_or_default();
                    let status = result.get("status").and_then(Value::as_u64).unwrap_or(0);
                    // A delete of a missing document is not a failure.
                    if status < 300 || (status == 404 && item.get("delete").is_some()) {
                        continue;
                    }
                    if retryable_status(status) && attempt < self.limits.max_retries {
                        retry.push(enc);
                    } else {
                        self.report.failed += 1;
                        if self.report.first_error.is_none() {
                            self.report.first_error = Some(
                                result
                                    .get("error")
                                    .map(|e| e.to_string())
                                    .unwrap_or_else(|| format!("status {status}")),
                            );
                        }
                    }
                }
            } else {
                // Success: grow back toward the configured maximum.
                self.target = (self.target * 2).min(self.limits.max_actions);
            }
            if !retry.is_empty() {
                attempt += 1;
                self.report.retried_items += retry.len();
                std::thread::sleep(self.backoff(attempt));
            }
            retry.extend(rest);
            batch = retry;
        }
        Ok(())
    }

    fn backoff(&self, attempt: u32) -> Duration {
        let factor = 1u32 << attempt.min(6);
        (self.limits.base_backoff * factor).min(Duration::from_secs(10))
    }

    /// Flushes and returns the report; any terminal failure is an error.
    pub fn finish(mut self) -> Result<BulkReport> {
        self.flush()?;
        if self.report.failed > 0 {
            return Err(BackendError::Bulk {
                failed: self.report.failed,
                total: self.report.actions,
                first_error: self.report.first_error.clone().unwrap_or_default(),
            });
        }
        Ok(self.report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{Counting, Response, Transport};
    use std::sync::Mutex;

    /// Scripted transport: responds per call from a queue, records bodies.
    struct Script {
        responses: Mutex<Vec<(u16, Value)>>,
        seen: Mutex<Vec<usize>>,
    }

    impl Transport for Script {
        fn send(&self, _m: Method, _p: &str, body: Option<(&[u8], &str)>) -> Result<Response> {
            let lines = body.map_or(0, |(b, _)| b.iter().filter(|c| **c == b'\n').count());
            self.seen.lock().unwrap().push(lines / 2);
            let (status, json) = {
                let mut r = self.responses.lock().unwrap();
                if r.is_empty() {
                    (200, serde_json::json!({"errors": false, "items": []}))
                } else {
                    r.remove(0)
                }
            };
            Ok(Response {
                status,
                body: serde_json::to_vec(&json).unwrap(),
            })
        }
    }

    fn client(responses: Vec<(u16, Value)>) -> Client {
        Counting::new(Box::new(Script {
            responses: Mutex::new(responses),
            seen: Mutex::new(vec![]),
        }) as Box<dyn Transport>)
    }

    fn limits(actions: usize, bytes: usize) -> BulkLimits {
        BulkLimits {
            max_actions: actions,
            max_bytes: bytes,
            max_retries: 3,
            base_backoff: Duration::from_millis(1),
        }
    }

    fn idx(i: usize) -> Action {
        Action::Index {
            index: "i".into(),
            id: i.to_string(),
            doc: serde_json::json!({"n": i}),
        }
    }

    #[test]
    fn batches_respect_action_and_byte_limits() {
        let c = client(vec![]);
        let mut w = BulkWriter::new(&c, limits(10, 1_000_000));
        for i in 0..25 {
            w.push(&idx(i)).unwrap();
        }
        let r = w.finish().unwrap();
        assert_eq!((r.actions, r.requests), (25, 3));
        assert_eq!(c.stats().bulk_actions, 25);

        let c = client(vec![]);
        // Each action is ~43 bytes of NDJSON -> at most 2 per 100-byte request.
        let mut w = BulkWriter::new(&c, limits(100, 100));
        for i in 0..6 {
            w.push(&idx(i)).unwrap();
        }
        assert_eq!(w.finish().unwrap().requests, 3);
        let st = c.stats();
        assert!(
            st.max_bulk_bytes <= 100,
            "byte bound: {}",
            st.max_bulk_bytes
        );
        assert!(st.max_bulk_actions <= 2);
        assert_eq!(st.bulk_actions, 6);
    }

    #[test]
    fn retryable_items_are_retried_and_terminal_ones_fail() {
        let partial = serde_json::json!({"errors": true, "items": [
            {"index": {"status": 201}},
            {"index": {"status": 429, "error": {"type": "es_rejected_execution_exception"}}},
        ]});
        let c = client(vec![(200, partial)]);
        let mut w = BulkWriter::new(&c, limits(2, 1_000_000));
        w.push(&idx(1)).unwrap();
        w.push(&idx(2)).unwrap();
        let r = w.finish().unwrap();
        assert_eq!((r.requests, r.retried_items, r.failed), (2, 1, 0));

        let bad = serde_json::json!({"errors": true, "items": [
            {"index": {"status": 400, "error": {"type": "strict_dynamic_mapping_exception"}}},
        ]});
        let c = client(vec![(200, bad)]);
        let mut w = BulkWriter::new(&c, limits(1, 1_000_000));
        w.push(&idx(1)).unwrap();
        let err = w.finish().unwrap_err();
        assert!(err.to_string().contains("strict_dynamic_mapping_exception"));
    }

    #[test]
    fn whole_request_rejection_shrinks_batches() {
        let c = client(vec![(429, Value::Null)]);
        let mut w = BulkWriter::new(&c, limits(4, 1_000_000));
        for i in 0..4 {
            w.push(&idx(i)).unwrap();
        }
        let r = w.finish().unwrap();
        // One rejected request of 4, then two of 2.
        assert_eq!(r.requests, 3);
    }

    #[test]
    fn deleting_missing_documents_is_not_an_error() {
        let resp = serde_json::json!({"errors": true, "items": [{"delete": {"status": 404, "result": "not_found"}}]});
        let c = client(vec![(200, resp)]);
        let mut w = BulkWriter::new(&c, limits(5, 1_000_000));
        w.push(&Action::Delete {
            index: "i".into(),
            id: "x".into(),
        })
        .unwrap();
        assert_eq!(w.finish().unwrap().failed, 0);
    }
}
