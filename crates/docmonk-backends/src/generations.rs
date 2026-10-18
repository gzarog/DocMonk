//! Generation visibility of server records (ADR 0035).
//!
//! Every build-scoped record is immutable once written and carries the
//! interval `[valid_from, valid_to)` of generations it belongs to, counted
//! in its *lineage*: the base indexes (files, code, documents, chunks)
//! share the source's base lineage, the relationships index (edges and
//! links) its graph lineage. A reader pinned to generation `G` sees a
//! record when `valid_from <= G < valid_to`.
//!
//! A lineage lives in the source-state document (`mvcc.base`,
//! `mvcc.graph`):
//!
//! * `next`: the counter. Every publish **attempt** allocates a fresh
//!   generation, so two attempts (a superseded writer and its successor)
//!   never share one.
//! * `pending`: the attempt in progress; `published`: the generation
//!   readers use.
//! * `aborted`: attempts that never published. Readers ignore records
//!   **written** by an aborted attempt (`valid_from` in the set) and the
//!   **closes** it made (`valid_to` in the set counts as still open). An
//!   attempt is aborted the moment a newer one begins or it fails, so
//!   whatever a superseded writer still does after losing its lease never
//!   becomes visible: the fenced compare-and-swap that sets `published`
//!   is the only visibility switch. Cleaning an aborted attempt's records
//!   up is housekeeping, never a correctness step.
//! * `ids`: build or graph-generation id -> generation, for the
//!   published, pending and retired (still readable) ones.
//!
//! Publishing generation `g` over `G` writes the changed and new files'
//! records with `valid_from = g` (invisible at `G`) and closes the
//! replaced and deleted files' records with `valid_to = g` (still visible
//! at `G`, since `g > G`); unchanged records are never touched.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::schema::IndexKind;

/// `valid_to` of a record no generation has replaced.
pub const OPEN: i64 = i64::MAX;

/// Painless: set `valid_to` to `params.v` (a close or a reopen).
pub(crate) const SET_VALID_TO: &str = "ctx._source.valid_to = params.v";

/// Which counter a record belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Line {
    /// Files, code, documents and chunks.
    Base,
    /// Relationship edges and links (never manual links).
    Graph,
}

impl Line {
    pub fn key(self) -> &'static str {
        match self {
            Line::Base => "base",
            Line::Graph => "graph",
        }
    }

    pub fn kinds(self) -> &'static [IndexKind] {
        match self {
            Line::Base => &[
                IndexKind::Files,
                IndexKind::Code,
                IndexKind::Documents,
                IndexKind::Chunks,
            ],
            Line::Graph => &[IndexKind::Relationships],
        }
    }

    /// Extra filter restricting a by-query operation to this lineage's
    /// records (manual links live in the relationships index too).
    pub fn record_filter(self) -> Option<Value> {
        match self {
            Line::Base => None,
            Line::Graph => Some(json!({ "terms": { "record_kind": ["edge", "link"] } })),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Aborted {
    pub generation: i64,
    /// Epoch millis when it was aborted.
    pub at: u128,
    /// Its records were swept (deleted / reopened) after it was aborted.
    #[serde(default)]
    pub swept: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lineage {
    /// The generation readers use (0: nothing published yet).
    #[serde(default)]
    pub published: i64,
    #[serde(default)]
    pub next: i64,
    #[serde(default)]
    pub pending: Option<i64>,
    #[serde(default)]
    pub ids: BTreeMap<String, i64>,
    #[serde(default)]
    pub aborted: Vec<Aborted>,
}

impl Lineage {
    /// The lineage stored in a state document (empty when absent).
    pub fn of(doc: &Value, line: Line) -> Self {
        doc.get("mvcc")
            .and_then(|m| m.get(line.key()))
            .and_then(|l| serde_json::from_value(l.clone()).ok())
            .unwrap_or_default()
    }

    pub fn put(&self, doc: &mut Map<String, Value>, line: Line) {
        let m = doc.entry("mvcc").or_insert_with(|| json!({}));
        if !m.is_object() {
            *m = json!({});
        }
        if let Some(m) = m.as_object_mut() {
            m.insert(
                line.key().into(),
                serde_json::to_value(self).unwrap_or(Value::Null),
            );
        }
    }

    pub fn aborted_generations(&self) -> Vec<i64> {
        self.aborted.iter().map(|a| a.generation).collect()
    }

    fn abort(&mut self, g: i64, now: u128) {
        if !self.aborted.iter().any(|a| a.generation == g) {
            self.aborted.push(Aborted {
                generation: g,
                at: now,
                swept: false,
            });
        }
        self.ids.retain(|_, v| *v != g);
    }

    /// Allocates a fresh generation for attempt `id`. A pending attempt
    /// it replaces is aborted.
    pub fn begin(&mut self, id: &str, now: u128) -> i64 {
        if let Some(p) = self.pending.take() {
            self.abort(p, now);
        }
        let g = self.next.max(self.published + 1).max(1);
        self.next = g + 1;
        self.ids.insert(id.to_owned(), g);
        self.pending = Some(g);
        g
    }

    /// Aborts the pending attempt `g` (if it is the pending one).
    pub fn abort_pending(&mut self, g: i64, now: u128) {
        if self.pending == Some(g) {
            self.pending = None;
        }
        self.abort(g, now);
    }

    /// Publishes the pending attempt `id`: returns its generation, or
    /// `None` when `id` is not the pending attempt.
    pub fn publish(&mut self, id: &str) -> Option<i64> {
        let g = *self.ids.get(id)?;
        if self.pending != Some(g) {
            return None;
        }
        self.pending = None;
        self.published = g;
        Some(g)
    }

    /// Keeps only the ids still needed (published, pending, `keep`).
    pub fn retain_ids(&mut self, keep: &[&str]) {
        let (published, pending) = (self.published, self.pending);
        self.ids
            .retain(|id, g| *g == published || Some(*g) == pending || keep.contains(&id.as_str()));
    }

    /// The generation of `id`, if it is still readable or pending.
    pub fn generation_of(&self, id: &str) -> Option<i64> {
        self.ids.get(id).copied()
    }

    /// The filter matching exactly the records visible at `g`.
    pub fn visible_at(&self, g: i64) -> Value {
        visible_at(g, &self.aborted_generations())
    }
}

/// Records visible at generation `g`, given the lineage's aborted
/// attempts: written by a non-aborted generation at or before `g`, and not
/// closed by a non-aborted generation at or before `g`.
pub fn visible_at(g: i64, aborted: &[i64]) -> Value {
    let mut open = vec![json!({ "range": { "valid_to": { "gt": g } } })];
    if !aborted.is_empty() {
        open.push(json!({ "terms": { "valid_to": aborted } }));
    }
    let mut b = json!({ "bool": { "filter": [
        { "range": { "valid_from": { "lte": g } } },
        { "bool": { "should": open, "minimum_should_match": 1 } },
    ] } });
    if !aborted.is_empty() {
        b["bool"]["must_not"] = json!([ { "terms": { "valid_from": aborted } } ]);
    }
    b
}

/// Records that are still open for a writer about to close them: never
/// closed, or only closed by an aborted attempt.
pub(crate) fn open_filter(aborted: &[i64]) -> Value {
    let mut open = vec![json!({ "term": { "valid_to": OPEN } })];
    if !aborted.is_empty() {
        open.push(json!({ "terms": { "valid_to": aborted } }));
    }
    json!({ "bool": { "should": open, "minimum_should_match": 1 } })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether a record `[from, to)` is visible at `g` under `aborted`,
    /// evaluated the way the filter is meant to be.
    fn visible(from: i64, to: i64, g: i64, aborted: &[i64]) -> bool {
        from <= g && !aborted.contains(&from) && (to > g || aborted.contains(&to))
    }

    #[test]
    fn every_attempt_gets_its_own_generation() {
        let mut l = Lineage::default();
        assert_eq!(l.begin("b1", 0), 1);
        assert_eq!(l.publish("b1"), Some(1));
        // A crashed attempt is replaced: it is aborted, never reused.
        assert_eq!(l.begin("b2", 0), 2);
        assert_eq!(l.begin("b3", 0), 3);
        assert_eq!(l.aborted_generations(), vec![2]);
        assert_eq!(l.publish("b2"), None, "an aborted attempt cannot publish");
        assert_eq!(l.publish("b3"), Some(3));
        l.retain_ids(&["b1"]);
        assert_eq!(l.generation_of("b1"), Some(1));
        assert_eq!(l.generation_of("b2"), None);
    }

    #[test]
    fn aborted_writes_and_closes_never_change_visibility() {
        // Published 1; attempt 2 wrote a new version of file f and closed
        // the old one, then was aborted; attempt 3 published without
        // touching f.
        let aborted = [2];
        let old = (1, 2); // closed by the aborted attempt
        let stale_new = (2, OPEN);
        assert!(visible(old.0, old.1, 3, &aborted), "the old version stays");
        assert!(!visible(stale_new.0, stale_new.1, 3, &aborted));
        // The real replacement by generation 4 hides it.
        assert!(!visible(1, 4, 4, &aborted));
        assert!(visible(1, 4, 3, &aborted), "readers of 3 keep it");
    }

    #[test]
    fn the_filter_has_the_intended_shape() {
        let f = visible_at(3, &[2]);
        assert_eq!(f["bool"]["filter"][0]["range"]["valid_from"]["lte"], 3);
        assert_eq!(
            f["bool"]["filter"][1]["bool"]["should"][0]["range"]["valid_to"]["gt"],
            3
        );
        assert_eq!(f["bool"]["must_not"][0]["terms"]["valid_from"][0], 2);
        assert!(visible_at(3, &[])
            .get("bool")
            .unwrap()
            .get("must_not")
            .is_none());
    }
}
