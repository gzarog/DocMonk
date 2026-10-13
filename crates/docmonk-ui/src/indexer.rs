//! The UI's background indexer: at most one index or rebuild run at a
//! time on a worker thread, recording progress events the
//! `/events/indexing` stream tails.
//!
//! Each source pass takes the same `index` lock as `docmonk index`, so a
//! UI run and a CLI run never index the same home concurrently.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use docmonk_core::paths::Home;
use serde_json::{json, Map, Value};

const MAX_EVENTS: usize = 500;

#[derive(Default)]
struct State {
    active: bool,
    seq: u64,
    events: VecDeque<Value>,
    last_summary: Option<Value>,
}

#[derive(Clone, Default)]
pub struct Indexer {
    state: Arc<Mutex<State>>,
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

impl Indexer {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn is_running(&self) -> bool {
        self.lock().active
    }

    pub fn last_summary(&self) -> Value {
        self.lock().last_summary.clone().unwrap_or(Value::Null)
    }

    /// Events after `seq`, oldest first.
    pub fn events_since(&self, seq: u64) -> Vec<Value> {
        self.lock()
            .events
            .iter()
            .filter(|e| e["seq"].as_u64().unwrap_or(0) > seq)
            .cloned()
            .collect()
    }

    fn emit(&self, kind: &str, payload: Value) {
        let mut st = self.lock();
        st.seq += 1;
        let mut event = Map::new();
        event.insert("seq".into(), json!(st.seq));
        event.insert("kind".into(), json!(kind));
        event.insert("at".into(), json!(now()));
        if let Value::Object(p) = payload {
            event.extend(p);
        }
        if st.events.len() == MAX_EVENTS {
            st.events.pop_front();
        }
        st.events.push_back(Value::Object(event));
    }

    /// Starts a run; `false` when one is already running.
    pub fn start(&self, home: Home, source_id: Option<String>, rebuild: bool, fresh: bool) -> bool {
        {
            let mut st = self.lock();
            if st.active {
                return false;
            }
            st.active = true;
        }
        let this = self.clone();
        std::thread::Builder::new()
            .name("docmonk-ui-indexer".into())
            .spawn(move || this.run(&home, source_id.as_deref(), rebuild, fresh))
            .map(|_| true)
            .unwrap_or_else(|_| {
                self.lock().active = false;
                false
            })
    }

    fn run(&self, home: &Home, source_id: Option<&str>, rebuild: bool, fresh: bool) {
        let mode = if rebuild { "rebuild" } else { "index" };
        self.emit(
            "run_started",
            json!({"mode": mode, "source_id": source_id, "fresh": fresh}),
        );
        let outcome = if rebuild {
            self.emit(
                "rebuild_started",
                json!({"source_id": source_id, "fresh": fresh}),
            );
            docmonk_ops::rebuild::rebuild_sources(home, source_id, fresh, |o| {
                self.emit(
                    "scan_completed",
                    json!({
                        "source_id": o["id"],
                        "path": o["path"],
                        "scanned": o["scanned"],
                        "indexed": o["indexed"],
                        "failed": o["failed"],
                    }),
                );
            })
            .map(|outcomes| summary("rebuild", &outcomes))
        } else {
            self.index(home, source_id)
        };
        let summary = match outcome {
            Ok(s) => {
                self.emit("run_completed", s.clone());
                s
            }
            Err(e) => {
                self.emit("run_failed", json!({"error": e.message()}));
                json!({"mode": mode, "sources": [], "failed": 0, "error": e.message()})
            }
        };
        let mut st = self.lock();
        st.active = false;
        st.last_summary = Some(summary);
    }

    fn index(
        &self,
        home: &Home,
        source_id: Option<&str>,
    ) -> Result<Value, docmonk_core::errors::DocMonkError> {
        use docmonk_service::indexing::{index_sources, selected_sources, SourceEvent};
        let sources = selected_sources(home, source_id)?;
        let mut rows = Vec::new();
        index_sources(home, &sources, "index", |event| match event {
            SourceEvent::Started { source } => self.emit(
                "scan_started",
                json!({"source_id": source.id, "path": source.path}),
            ),
            SourceEvent::Blocked { source, error } | SourceEvent::Failed { source, error } => {
                let row = json!({
                    "source_id": source.id,
                    "path": source.path,
                    "error": error.message(),
                    "failed": 0,
                });
                self.emit("scan_failed", row.clone());
                rows.push(row);
            }
            SourceEvent::Completed { source, run } => {
                let row = json!({
                    "source_id": source.id,
                    "path": source.path,
                    "offline": run.offline.is_some(),
                    "scanned": run.counts.scanned,
                    "indexed": run.indexed,
                    "failed": run.failed,
                });
                self.emit("scan_completed", row.clone());
                rows.push(row);
            }
            SourceEvent::Dependencies { source, outcome } => {
                let mut row = json!(outcome);
                row["source_id"] = json!(source.id);
                self.emit("dependencies_completed", row);
            }
            // Phase 2 (after every source indexed): reported on its own,
            // never as an indexing failure.
            SourceEvent::Relationships { source, outcome } => {
                let mut row = outcome.json();
                row["source_id"] = json!(source.id);
                self.emit(
                    if outcome.is_failure() {
                        "relationships_failed"
                    } else {
                        "relationships_completed"
                    },
                    row.clone(),
                );
                if let Some(r) = rows
                    .iter_mut()
                    .find(|r| r["source_id"] == source.id.as_str())
                {
                    if let (Some(r), Some(extra)) = (r.as_object_mut(), row.as_object()) {
                        for k in ["relationship_state", "relationship_error"] {
                            if let Some(v) = extra.get(k) {
                                r.insert(k.into(), v.clone());
                            }
                        }
                    }
                }
            }
        })?;
        Ok(summary("index", &rows))
    }
}

fn summary(mode: &str, rows: &[Value]) -> Value {
    let failed: i64 = rows.iter().map(|r| r["failed"].as_i64().unwrap_or(0)).sum();
    let sources: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mut r = r.clone();
            if let Some(o) = r.as_object_mut() {
                if let Some(id) = o.remove("id") {
                    o.insert("source_id".into(), id);
                }
                o.remove("linked");
            }
            r
        })
        .collect();
    json!({"mode": mode, "sources": sources, "failed": failed})
}
