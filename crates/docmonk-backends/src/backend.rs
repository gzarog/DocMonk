//! The server backend: schema init, build-scoped writes, atomic
//! publication, visible-build reads and cleanup.
//!
//! Publication protocol per source:
//! 1. `begin_build` records `pending_build_id` in the source-state index
//!    (and deletes the leftovers of any earlier abandoned pending build).
//! 2. Records are bulk-written with `build_id = <pending>` and `_id =
//!    "<build_id>:<record id>"` (deterministic and idempotent).
//! 3. `publish_build` refreshes the build-scoped indexes, then switches
//!    `active_build_id` with an optimistic-concurrency write
//!    (`if_seq_no`/`if_primary_term`), then garbage-collects other builds.
//! 4. Every read filters to `(source_id, active_build_id)` pairs, so a
//!    pending or failed build is never visible.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::bulk::{Action, BulkLimits, BulkReport, BulkWriter};
use crate::engine::{Engine, VectorSpec};
use crate::error::{BackendError, Result};
use crate::schema::{self, IndexKind, IndexSettings};
use crate::transport::{Auth, Client, Counting, HttpTransport, Method, RequestStats, Response};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct InitReport {
    pub created: Vec<String>,
    pub existing: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SourceState {
    pub source_id: String,
    pub path: Option<String>,
    pub state: String,
    pub active_build_id: Option<String>,
    pub pending_build_id: Option<String>,
    pub versions: Value,
    /// The whole stored document (catalog fields, lease, retired builds).
    pub doc: Value,
    pub(crate) seq_no: i64,
    pub(crate) primary_term: i64,
}

impl SourceState {
    /// A field of the stored document as a string.
    pub fn field(&self, key: &str) -> Option<&str> {
        self.doc.get(key).and_then(Value::as_str)
    }
}

/// A source registration in the server catalog.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CatalogEntry {
    pub source_id: String,
    /// Canonical root as seen by the indexing host.
    pub path: String,
    pub source_type: String,
    pub enabled: bool,
    pub include_patterns: Vec<String>,
    pub exclude_patterns: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl CatalogEntry {
    pub fn from_doc(doc: &Value) -> Self {
        let s = |k: &str| {
            doc.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let list = |k: &str| -> Vec<String> {
            match doc.get(k) {
                Some(Value::Array(a)) => a
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
                Some(Value::String(one)) => vec![one.clone()],
                _ => Vec::new(),
            }
        };
        Self {
            source_id: s("source_id"),
            path: s("path"),
            source_type: s("source_type"),
            enabled: doc.get("enabled").and_then(Value::as_bool).unwrap_or(true),
            include_patterns: list("include_patterns"),
            exclude_patterns: list("exclude_patterns"),
            created_at: s("created_at"),
            updated_at: s("updated_at"),
        }
    }
}

/// A source's writer lease: who holds it, its fencing token and its TTL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub source_id: String,
    pub owner: String,
    pub token: i64,
    pub ttl: Duration,
}

/// Painless: drop build `params.b` from a record; delete the record when
/// no build is left.
pub(crate) const DETACH_SCRIPT: &str = "def v = ctx._source.build_id; List l = new ArrayList(); \
    if (v instanceof List) { l.addAll(v) } else if (v != null) { l.add(v) } \
    l.removeIf(x -> x == params.b); \
    if (l.isEmpty()) { ctx.op = 'delete' } else { ctx._source.build_id = l }";

/// Painless: add build `params.b` to a record (copy-forward), keeping only
/// build ids still in `params.keep`.
pub(crate) const ATTACH_SCRIPT: &str = "def v = ctx._source.build_id; List l = new ArrayList(); \
    if (v instanceof List) { l.addAll(v) } else if (v != null) { l.add(v) } \
    l.removeIf(x -> !params.keep.contains(x)); \
    if (!l.contains(params.b)) { l.add(params.b) } \
    ctx._source.build_id = l";

/// `record_kind` of manual knowledge links: not build-scoped.
pub const MANUAL_LINK: &str = "manual_link";
/// `build_id` stored on manual links.
pub const MANUAL_BUILD: &str = "manual";

/// Page size of every scan over the source-state index.
pub const SCAN_PAGE: usize = 1000;

pub(crate) fn check_lease(state: &SourceState, lease: Option<&Lease>) -> Result<()> {
    let owner = state.doc["lease_owner"].as_str().unwrap_or_default();
    let token = state.doc["lease_token"].as_i64().unwrap_or(0);
    let live = state.doc["lease_expires_at"]
        .as_str()
        .and_then(|s| s.parse::<u128>().ok())
        .is_some_and(|e| e > now_millis());
    match lease {
        Some(l) => {
            if owner != l.owner || token != l.token {
                return Err(BackendError::Conflict(format!(
                    "writer lease of {} was superseded (token {} is now {token} held by {owner:?})",
                    l.source_id, l.token
                )));
            }
            if !live {
                return Err(BackendError::Conflict(format!(
                    "writer lease of {} expired; refusing to change its state",
                    l.source_id
                )));
            }
            Ok(())
        }
        None if !owner.is_empty() && live => Err(BackendError::Conflict(format!(
            "source {} is being indexed by {owner}; its writer lease is still live",
            state.source_id
        ))),
        None => Ok(()),
    }
}

fn retired_builds(doc: &Value) -> Vec<(String, u128)> {
    doc.get("retired_builds")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| {
            Some((
                r["build_id"].as_str()?.to_owned(),
                r["retired_at"].as_str()?.parse().ok()?,
            ))
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SearchHit {
    pub id: String,
    pub source_id: String,
    pub rel_path: String,
    pub score: f64,
}

pub struct ServerBackend {
    client: Client,
    engine: Engine,
    prefix: String,
    vector: Option<VectorSpec>,
    limits: BulkLimits,
    settings: IndexSettings,
    /// How long a replaced build stays readable after a publish.
    gc_grace: Duration,
    /// Longest one by-query task may run before it is cancelled.
    task_budget: Duration,
    /// Server-side wait of one task poll (below the request timeout).
    poll_wait: Duration,
}

/// Default [`ServerBackend::with_task_limits`] budget.
pub const DEFAULT_TASK_BUDGET: Duration = Duration::from_secs(3600);
/// Default server-side wait per task poll.
pub const DEFAULT_POLL_WAIT: Duration = Duration::from_secs(10);

fn ok(resp: Response, method: &str, path: &str) -> Result<Value> {
    if resp.status >= 300 {
        return Err(BackendError::Http {
            method: method.into(),
            path: path.into(),
            status: resp.status,
            reason: String::from_utf8_lossy(&resp.body)
                .chars()
                .take(400)
                .collect(),
        });
    }
    resp.json()
}

fn body(v: &Value) -> Vec<u8> {
    serde_json::to_vec(v).unwrap_or_default()
}

impl ServerBackend {
    /// Connects using `storage.server` config, detecting the engine and
    /// refusing a cluster that does not match the configured engine.
    pub fn connect(
        cfg: &docmonk_config::model::ServerStorageConfig,
        vector: Option<VectorSpec>,
    ) -> Result<Self> {
        if cfg.url.is_empty() {
            return Err(BackendError::Invalid("storage.server.url is empty".into()));
        }
        let engine_name = cfg.engine.as_str();
        let transport = HttpTransport::new(
            &cfg.url,
            Auth::from_env(engine_name),
            Duration::from_secs_f64(cfg.request_timeout_seconds.max(0.001)),
            cfg.verify_tls,
        )?;
        let client: Client = Counting::new(Box::new(transport));
        let root = ok(client.send(Method::Get, "/", None)?, "GET", "/")?;
        let engine = Engine::detect(&root)
            .ok_or_else(|| BackendError::Invalid("could not detect the server engine".into()))?;
        if engine.as_str() != engine_name {
            return Err(BackendError::Invalid(format!(
                "storage.server.engine is {engine_name} but the cluster is {}",
                engine.as_str()
            )));
        }
        let request_timeout = Duration::from_secs_f64(cfg.request_timeout_seconds.max(0.001));
        Ok(Self::with_client(
            client,
            engine,
            &cfg.index_prefix,
            vector,
            BulkLimits::from_config(&cfg.bulk),
        )
        .with_task_limits(
            Duration::from_secs_f64(cfg.task_timeout_seconds.max(1.0)),
            (request_timeout / 2).min(DEFAULT_POLL_WAIT),
        ))
    }

    pub fn with_client(
        client: Client,
        engine: Engine,
        prefix: &str,
        vector: Option<VectorSpec>,
        limits: BulkLimits,
    ) -> Self {
        Self {
            client,
            engine,
            prefix: prefix.to_owned(),
            vector,
            limits,
            settings: IndexSettings::default(),
            gc_grace: Duration::ZERO,
            task_budget: DEFAULT_TASK_BUDGET,
            poll_wait: DEFAULT_POLL_WAIT,
        }
    }

    /// Sets the by-query task budget and the server-side wait per poll
    /// (keep it below the client request timeout).
    pub fn with_task_limits(mut self, budget: Duration, poll_wait: Duration) -> Self {
        self.task_budget = budget;
        self.poll_wait = poll_wait.max(Duration::from_secs(1));
        self
    }

    /// Keeps replaced builds readable for `grace` after a publish.
    pub fn with_gc_grace(mut self, grace: Duration) -> Self {
        self.gc_grace = grace;
        self
    }

    /// How long a replaced build (or graph generation) stays readable.
    pub fn gc_grace(&self) -> Duration {
        self.gc_grace
    }

    pub fn vector_spec(&self) -> Option<&VectorSpec> {
        self.vector.as_ref()
    }

    pub fn with_settings(mut self, settings: IndexSettings) -> Self {
        self.settings = settings;
        self
    }

    pub fn engine(&self) -> Engine {
        self.engine
    }
    pub fn prefix(&self) -> &str {
        &self.prefix
    }
    pub fn stats(&self) -> RequestStats {
        self.client.stats()
    }
    pub fn client(&self) -> &Client {
        &self.client
    }
    pub fn index(&self, kind: IndexKind) -> String {
        schema::index_name(&self.prefix, kind)
    }

    fn call(&self, method: Method, path: &str, payload: Option<&Value>) -> Result<Response> {
        match payload {
            Some(v) => {
                let b = body(v);
                self.client
                    .send(method, path, Some((&b, "application/json")))
            }
            None => self.client.send(method, path, None),
        }
    }

    /// `DELETE path`: `Ok(false)` when the target did not exist.
    pub fn delete_path(&self, path: &str) -> Result<bool> {
        let resp = self.call(Method::Delete, path, None)?;
        if resp.status == 404 {
            return Ok(false);
        }
        ok(resp, "DELETE", path).map(|_| true)
    }

    /// One JSON request, failing on a non-2xx status.
    pub fn post_json(&self, method: Method, path: &str, payload: &Value) -> Result<Value> {
        self.call_ok(method, path, Some(payload))
    }

    /// One `_msearch` of `(index, body)` searches: the per-search
    /// response objects in order (an `error` entry stays as returned).
    pub fn msearch(&self, searches: &[(String, Value)]) -> Result<Vec<Value>> {
        if searches.is_empty() {
            return Ok(Vec::new());
        }
        let mut payload = Vec::new();
        for (index, b) in searches {
            payload.extend(body(&serde_json::json!({ "index": index })));
            payload.push(b'\n');
            payload.extend(body(b));
            payload.push(b'\n');
        }
        let resp = self.client.send(
            Method::Post,
            "/_msearch",
            Some((&payload, "application/x-ndjson")),
        )?;
        let v = ok(resp, "POST", "/_msearch")?;
        Ok(v["responses"].as_array().cloned().unwrap_or_default())
    }

    /// `GET path` (no body), failing on a non-2xx status.
    pub fn get_json(&self, path: &str) -> Result<Value> {
        self.call_ok(Method::Get, path, None)
    }

    fn call_ok(&self, method: Method, path: &str, payload: Option<&Value>) -> Result<Value> {
        let m = format!("{method:?}").to_uppercase();
        ok(self.call(method, path, payload)?, &m, path)
    }

    // ---- schema --------------------------------------------------------

    /// Creates missing DocMonk indexes and verifies existing ones. Never
    /// modifies an existing index.
    pub fn init(&self) -> Result<InitReport> {
        let mut report = InitReport {
            created: vec![],
            existing: vec![],
        };
        // Verify every existing index before creating anything, so an
        // incompatible prefix fails without leaving new indexes behind.
        let mut missing = Vec::new();
        for kind in IndexKind::ALL {
            let name = self.index(kind);
            let head = self.call(Method::Head, &format!("/{name}"), None)?;
            if head.status == 404 {
                missing.push(kind);
            } else {
                self.verify(kind)?;
                report.existing.push(name);
            }
        }
        for kind in missing {
            let name = self.index(kind);
            let create =
                schema::create_body(kind, self.engine, self.vector.as_ref(), &self.settings);
            let resp = self.call(Method::Put, &format!("/{name}"), Some(&create))?;
            let raced = resp.status == 400
                && String::from_utf8_lossy(&resp.body)
                    .contains("resource_already_exists_exception");
            if raced {
                self.verify(kind)?;
                report.existing.push(name);
            } else {
                ok(resp, "PUT", &format!("/{name}"))?;
                report.created.push(name);
            }
        }
        self.wait_for_shards()?;
        Ok(report)
    }

    /// Verifies every DocMonk index's schema identity (never creates or
    /// changes anything). Missing indexes are reported by
    /// [`Self::index_status`].
    pub fn verify_schema(&self) -> Result<()> {
        for kind in IndexKind::ALL {
            self.verify(kind)?;
        }
        Ok(())
    }

    /// One request: the names of missing DocMonk indexes, after verifying
    /// the schema identity of every existing one (never creates or
    /// changes anything).
    pub fn check_schema(&self) -> Result<Vec<String>> {
        let names: Vec<String> = IndexKind::ALL.iter().map(|k| self.index(*k)).collect();
        let v = self.call_ok(
            Method::Get,
            &format!(
                "/{}/_mapping?ignore_unavailable=true&allow_no_indices=true",
                names.join(",")
            ),
            None,
        )?;
        let mut missing = Vec::new();
        for (kind, name) in IndexKind::ALL.iter().zip(&names) {
            match v.get(name) {
                Some(m) => schema::check_meta(
                    name,
                    m.pointer("/mappings/_meta"),
                    *kind,
                    self.vector.as_ref(),
                )
                .map_err(BackendError::SchemaMismatch)?,
                None => missing.push(name.clone()),
            }
        }
        Ok(missing)
    }

    /// Whether each DocMonk index exists (`HEAD`; never creates anything).
    pub fn index_status(&self) -> Result<Vec<(String, bool)>> {
        let mut out = Vec::new();
        for kind in IndexKind::ALL {
            let name = self.index(kind);
            let head = self.call(Method::Head, &format!("/{name}"), None)?;
            out.push((name, head.status != 404));
        }
        Ok(out)
    }

    /// Waits until every DocMonk index has its primaries allocated (yellow), so
    /// the first write/read after creation does not hit an unassigned shard.
    fn wait_for_shards(&self) -> Result<()> {
        let names: Vec<String> = IndexKind::ALL.iter().map(|k| self.index(*k)).collect();
        let path = format!(
            "/_cluster/health/{}?wait_for_status=yellow&timeout=60s",
            names.join(",")
        );
        let v = self.call_ok(Method::Get, &path, None)?;
        if v["timed_out"].as_bool() == Some(true) {
            return Err(BackendError::Transport(format!(
                "DocMonk indexes did not become available (cluster health {}); check cluster disk/allocation",
                v["status"]
            )));
        }
        Ok(())
    }

    fn verify(&self, kind: IndexKind) -> Result<()> {
        let name = self.index(kind);
        let mapping = self.call_ok(Method::Get, &format!("/{name}/_mapping"), None)?;
        let meta = mapping
            .as_object()
            .and_then(|m| m.values().next())
            .and_then(|v| v.pointer("/mappings/_meta"));
        schema::check_meta(&name, meta, kind, self.vector.as_ref())
            .map_err(BackendError::SchemaMismatch)
    }

    // ---- source state ---------------------------------------------------

    fn state_from(&self, source_id: &str, v: &Value) -> SourceState {
        let src = &v["_source"];
        let s = |k: &str| src.get(k).and_then(Value::as_str).map(str::to_owned);
        SourceState {
            source_id: source_id.to_owned(),
            path: s("path"),
            state: s("state").unwrap_or_default(),
            active_build_id: s("active_build_id"),
            pending_build_id: s("pending_build_id"),
            versions: src.get("versions").cloned().unwrap_or(Value::Null),
            doc: src.clone(),
            seq_no: v["_seq_no"].as_i64().unwrap_or(-1),
            primary_term: v["_primary_term"].as_i64().unwrap_or(-1),
        }
    }

    pub fn source_state(&self, source_id: &str) -> Result<Option<SourceState>> {
        let path = format!(
            "/{}/_doc/{}",
            self.index(IndexKind::SourceState),
            encode_id(source_id)
        );
        let resp = self.call(Method::Get, &path, None)?;
        if resp.status == 404 {
            return Ok(None);
        }
        let v = ok(resp, "GET", &path)?;
        Ok(Some(self.state_from(source_id, &v)))
    }

    fn write_state(&self, prev: Option<&SourceState>, doc: &Value) -> Result<()> {
        let source_id = doc["source_id"].as_str().unwrap_or_default();
        let index = self.index(IndexKind::SourceState);
        let path = match prev {
            Some(p) => format!(
                "/{index}/_doc/{}?if_seq_no={}&if_primary_term={}&refresh=true",
                encode_id(source_id),
                p.seq_no,
                p.primary_term
            ),
            None => format!("/{index}/_create/{}?refresh=true", encode_id(source_id)),
        };
        let resp = self.call(Method::Put, &path, Some(doc))?;
        if resp.status == 409 {
            return Err(BackendError::Conflict(format!(
                "source {source_id} state changed concurrently; another DocMonk process is publishing it"
            )));
        }
        ok(resp, "PUT", &path).map(|_| ())
    }

    /// Read-modify-write of one source-state document under optimistic
    /// concurrency. `lease` (when given) must still be the source's live
    /// writer lease; without one, a live lease held by anybody refuses the
    /// write. `patch` edits the stored fields; everything else is kept.
    pub(crate) fn patch_state(
        &self,
        source_id: &str,
        lease: Option<&Lease>,
        create_path: Option<&str>,
        patch: impl FnOnce(&mut Map<String, Value>, Option<&SourceState>) -> Result<()>,
    ) -> Result<()> {
        let prev = self.source_state(source_id)?;
        if let Some(p) = &prev {
            check_lease(p, lease)?;
        } else if lease.is_some() {
            return Err(BackendError::Conflict(format!(
                "source {source_id} is not registered on the server"
            )));
        }
        let mut doc = match &prev {
            Some(p) => p.doc.as_object().cloned().unwrap_or_default(),
            None => {
                let mut m = Map::new();
                m.insert("source_id".into(), json!(source_id));
                m.insert("path".into(), json!(create_path));
                m.insert("state".into(), json!("needs_full_rebuild"));
                m.insert("enabled".into(), json!(true));
                m.insert("created_at".into(), json!(now()));
                m
            }
        };
        patch(&mut doc, prev.as_ref())?;
        doc.insert("updated_at".into(), json!(now()));
        self.write_state(prev.as_ref(), &Value::Object(doc))
    }

    // ---- writer leases ----------------------------------------------------

    /// Takes the source's writer lease for `ttl`. Fails with
    /// [`BackendError::Conflict`] while another owner's lease is live. Each
    /// grant increments the fencing token, so a superseded holder's later
    /// state writes (publish, abort) are refused.
    pub fn acquire_lease(&self, source_id: &str, owner: &str, ttl: Duration) -> Result<Lease> {
        let prev = self.source_state(source_id)?.ok_or_else(|| {
            BackendError::Conflict(format!(
                "source {source_id} is not registered on the server"
            ))
        })?;
        let now_ms = now_millis();
        let held_by = prev.doc["lease_owner"].as_str().unwrap_or_default();
        let expires = prev.doc["lease_expires_at"]
            .as_str()
            .and_then(|s| s.parse::<u128>().ok());
        if !held_by.is_empty() && held_by != owner && expires.is_some_and(|e| e > now_ms) {
            return Err(BackendError::Conflict(format!(
                "source {source_id} is being indexed by {held_by}; its writer lease is still live"
            )));
        }
        let token = prev.doc["lease_token"].as_i64().unwrap_or(0) + 1;
        let mut doc = prev.doc.as_object().cloned().unwrap_or_default();
        doc.insert("lease_owner".into(), json!(owner));
        doc.insert("lease_token".into(), json!(token));
        doc.insert(
            "lease_expires_at".into(),
            json!((now_ms + ttl.as_millis()).to_string()),
        );
        doc.insert("updated_at".into(), json!(now()));
        self.write_state(Some(&prev), &Value::Object(doc))?;
        Ok(Lease {
            source_id: source_id.to_owned(),
            owner: owner.to_owned(),
            token,
            ttl,
        })
    }

    /// Extends a live lease. Fails once it was superseded or expired.
    pub fn renew_lease(&self, lease: &Lease) -> Result<()> {
        self.patch_state(&lease.source_id, Some(lease), None, |doc, _| {
            doc.insert(
                "lease_expires_at".into(),
                json!((now_millis() + lease.ttl.as_millis()).to_string()),
            );
            Ok(())
        })
    }

    /// Gives a lease up. A lease that was already superseded is left alone.
    pub fn release_lease(&self, lease: &Lease) -> Result<()> {
        match self.patch_state(&lease.source_id, Some(lease), None, |doc, _| {
            doc.insert("lease_owner".into(), Value::Null);
            doc.insert("lease_expires_at".into(), Value::Null);
            Ok(())
        }) {
            Err(BackendError::Conflict(_)) => Ok(()),
            other => other,
        }
    }

    // ---- source catalog -----------------------------------------------------

    /// Registers a source in the server catalog (idempotent by id): an
    /// existing registration is returned unchanged with `created = false`.
    pub fn register_source(&self, entry: &CatalogEntry) -> Result<(CatalogEntry, bool)> {
        if let Some(existing) = self.catalog_entry(&entry.source_id)? {
            return Ok((existing, false));
        }
        let now = now();
        let doc = json!({
            "source_id": entry.source_id,
            "path": entry.path,
            "source_type": entry.source_type,
            "enabled": entry.enabled,
            "include_patterns": entry.include_patterns,
            "exclude_patterns": entry.exclude_patterns,
            "created_at": now,
            "state": "needs_full_rebuild",
            "rebuild_reason": "new source",
            "online_status": "unknown",
            "updated_at": now,
        });
        match self.write_state(None, &doc) {
            Ok(()) => {}
            // Lost a registration race: the other registration wins.
            Err(BackendError::Conflict(_)) | Err(BackendError::Http { status: 409, .. }) => {
                if let Some(existing) = self.catalog_entry(&entry.source_id)? {
                    return Ok((existing, false));
                }
            }
            Err(e) => return Err(e),
        }
        let created = self
            .catalog_entry(&entry.source_id)?
            .ok_or_else(|| BackendError::Invalid("registered source vanished".into()))?;
        Ok((created, true))
    }

    pub fn catalog_entry(&self, source_id: &str) -> Result<Option<CatalogEntry>> {
        Ok(self
            .source_state(source_id)?
            .map(|s| CatalogEntry::from_doc(&s.doc)))
    }

    /// Every registered source (optionally only enabled ones), paged with
    /// `search_after` in `source_id` order: no truncation at any size.
    pub fn list_catalog(&self, enabled_only: bool) -> Result<Vec<CatalogEntry>> {
        let query = if enabled_only {
            json!({ "term": { "enabled": true } })
        } else {
            json!({ "match_all": {} })
        };
        Ok(self
            .scan_states(query)?
            .iter()
            .map(CatalogEntry::from_doc)
            .collect())
    }

    pub fn set_source_enabled(&self, source_id: &str, enabled: bool) -> Result<()> {
        self.require_registered(source_id)?;
        self.patch_state(source_id, None, None, |doc, _| {
            doc.insert("enabled".into(), json!(enabled));
            Ok(())
        })
    }

    /// Records the outcome of a scan (online/offline, last error).
    pub fn set_online(
        &self,
        source_id: &str,
        lease: Option<&Lease>,
        online: bool,
        error: Option<&str>,
    ) -> Result<()> {
        self.patch_state(source_id, lease, None, |doc, _| {
            doc.insert(
                "online_status".into(),
                json!(if online { "online" } else { "offline" }),
            );
            doc.insert("last_scan_at".into(), json!(now()));
            doc.insert("last_error".into(), json!(error));
            Ok(())
        })
    }

    fn require_registered(&self, source_id: &str) -> Result<()> {
        if self.source_state(source_id)?.is_none() {
            return Err(BackendError::NotFound(format!(
                "no such source: {source_id}"
            )));
        }
        Ok(())
    }

    /// Source-state documents matching `query`, every page.
    pub(crate) fn scan_states(&self, query: Value) -> Result<Vec<Value>> {
        self.scan_state_docs(query, None)
    }

    // ---- builds -------------------------------------------------------------

    /// Starts a build. Leftovers of a previous abandoned pending build are
    /// deleted first; the active build stays visible throughout.
    pub fn begin_build(&self, source_id: &str, path: &str, build_id: &str) -> Result<()> {
        self.begin_build_with(None, source_id, path, build_id)
    }

    /// [`Self::begin_build`] fenced by `lease`.
    pub fn begin_build_with(
        &self,
        lease: Option<&Lease>,
        source_id: &str,
        path: &str,
        build_id: &str,
    ) -> Result<()> {
        if let Some(prev) = self.source_state(source_id)? {
            check_lease(&prev, lease)?;
            if let Some(old) = prev.pending_build_id.clone() {
                if old != build_id {
                    self.delete_build_docs(source_id, &old)?;
                }
            }
        }
        self.patch_state(source_id, lease, Some(path), |doc, _| {
            doc.insert("path".into(), json!(path));
            doc.insert("state".into(), json!("building"));
            doc.insert("pending_build_id".into(), json!(build_id));
            Ok(())
        })
    }

    /// Writer for records of one build.
    pub fn build_writer(&self, source_id: &str, build_id: &str) -> BuildWriter<'_> {
        BuildWriter {
            backend: self,
            source_id: source_id.to_owned(),
            build_id: build_id.to_owned(),
            bulk: BulkWriter::new(&self.client, self.limits),
        }
    }

    pub(crate) fn refresh_build_indexes(&self) -> Result<()> {
        let names: Vec<String> = IndexKind::BUILD_SCOPED
            .iter()
            .map(|k| self.index(*k))
            .collect();
        self.call_ok(
            Method::Post,
            &format!("/{}/_refresh", names.join(",")),
            None,
        )
        .map(|_| ())
    }

    /// Atomically makes `build_id` the visible build. The build it replaces
    /// is retired, not deleted: it stays readable for `gc_grace` so queries
    /// pinned to it finish, and is garbage-collected by a later publish.
    pub fn publish_build(
        &self,
        source_id: &str,
        build_id: &str,
        versions: &Value,
        full: bool,
    ) -> Result<()> {
        self.publish_build_with(None, source_id, build_id, versions, full)
    }

    /// [`Self::publish_build`] fenced by `lease`: a superseded or expired
    /// lease cannot publish.
    pub fn publish_build_with(
        &self,
        lease: Option<&Lease>,
        source_id: &str,
        build_id: &str,
        versions: &Value,
        full: bool,
    ) -> Result<()> {
        self.publish_build_ext(lease, source_id, build_id, versions, full, &[])
    }

    /// [`Self::publish_build_with`], also storing `extra` state fields in
    /// the same compare-and-swap write.
    pub fn publish_build_ext(
        &self,
        lease: Option<&Lease>,
        source_id: &str,
        build_id: &str,
        versions: &Value,
        full: bool,
        extra: &[(&str, Value)],
    ) -> Result<()> {
        let prev = self
            .source_state(source_id)?
            .ok_or_else(|| BackendError::Invalid(format!("no build started for {source_id}")))?;
        if prev.pending_build_id.as_deref() != Some(build_id) {
            return Err(BackendError::Conflict(format!(
                "build {build_id} is not the pending build of {source_id}"
            )));
        }
        check_lease(&prev, lease)?;
        self.refresh_build_indexes()?;
        let now_ms = now_millis();
        let mut retired = retired_builds(&prev.doc);
        if let Some(old) = prev.active_build_id.as_ref().filter(|b| *b != build_id) {
            retired.push((old.clone(), now_ms));
        }
        let grace = self.gc_grace.as_millis();
        let (keep, expired): (Vec<_>, Vec<_>) = retired
            .into_iter()
            .partition(|(_, at)| grace > 0 && now_ms.saturating_sub(*at) < grace);
        self.patch_state(source_id, lease, None, |doc, _| {
            doc.insert("state".into(), json!("ready"));
            doc.insert("active_build_id".into(), json!(build_id));
            doc.insert("pending_build_id".into(), Value::Null);
            // The graph record survives a base publication (the opaque
            // `versions` object carries it); readers treat a graph of
            // another base build as stale.
            let mut versions = versions.clone();
            if let (Some(v), Some(g)) = (
                versions.as_object_mut(),
                doc.get("versions")
                    .and_then(|v| v.get(crate::graph::GRAPH_KEY)),
            ) {
                v.insert(crate::graph::GRAPH_KEY.into(), g.clone());
            }
            doc.insert("versions".into(), versions);
            doc.insert("rebuild_reason".into(), Value::Null);
            doc.insert("published_at".into(), json!(now()));
            for (k, v) in extra {
                doc.insert((*k).into(), v.clone());
            }
            if full {
                doc.insert("last_full_build_at".into(), json!(now()));
            }
            doc.insert(
                "retired_builds".into(),
                json!(keep
                    .iter()
                    .map(|(b, at)| json!({ "build_id": b, "retired_at": at.to_string() }))
                    .collect::<Vec<_>>()),
            );
            Ok(())
        })?;
        let _ = expired;
        let mut keep_ids: Vec<String> = keep.into_iter().map(|(b, _)| b).collect();
        keep_ids.push(build_id.to_owned());
        self.gc_source(source_id, &keep_ids)
    }

    /// Discards a pending build. The previously active build (if any)
    /// stays visible.
    pub fn abort_build(&self, source_id: &str, build_id: &str) -> Result<()> {
        self.abort_build_with(None, source_id, build_id, None)
    }

    /// [`Self::abort_build`] fenced by `lease`, recording `error`. A
    /// superseded holder only deletes its own pending records; it never
    /// touches the state a newer holder owns.
    pub fn abort_build_with(
        &self,
        lease: Option<&Lease>,
        source_id: &str,
        build_id: &str,
        error: Option<&str>,
    ) -> Result<()> {
        let Some(prev) = self.source_state(source_id)? else {
            return Ok(());
        };
        if prev.active_build_id.as_deref() == Some(build_id) {
            return Err(BackendError::Conflict(format!(
                "build {build_id} is the published build of {source_id}; refusing to abort it"
            )));
        }
        if let Err(e) = check_lease(&prev, lease) {
            // Superseded: the build is someone else's pending build (never
            // touch it) or an orphan of this holder (safe to delete).
            if prev.pending_build_id.as_deref() == Some(build_id) {
                return Err(e);
            }
            self.delete_build_docs(source_id, build_id)?;
            return Ok(());
        }
        self.delete_build_docs(source_id, build_id)?;
        if prev.pending_build_id.as_deref() == Some(build_id) {
            self.patch_state(source_id, lease, None, |doc, p| {
                let has_active = p.is_some_and(|p| p.active_build_id.is_some());
                doc.insert(
                    "state".into(),
                    json!(if has_active {
                        "failed"
                    } else {
                        "needs_full_rebuild"
                    }),
                );
                doc.insert("pending_build_id".into(), Value::Null);
                if let Some(e) = error {
                    doc.insert("last_error".into(), json!(e));
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    /// Marks a source for a full rebuild on its next pass.
    pub fn require_full_rebuild(&self, source_id: &str, reason: &str) -> Result<()> {
        self.patch_state(source_id, None, None, |doc, _| {
            doc.insert("state".into(), json!("needs_full_rebuild"));
            doc.insert("rebuild_reason".into(), json!(reason));
            Ok(())
        })
    }

    /// Runs a `_update_by_query` / `_delete_by_query` as a detached task
    /// (`wait_for_completion=false`, `slices=auto`) and polls it with
    /// bounded waits, so a long server-side operation never trips the
    /// client request timeout. Returns the task's response (the same body
    /// a synchronous call returns). A task still running after the
    /// budget is cancelled. A server that answers inline (no task id) is
    /// accepted as is.
    pub(crate) fn by_query(&self, path: &str, payload: &Value) -> Result<Value> {
        let started = std::time::Instant::now();
        let sep = if path.contains('?') { '&' } else { '?' };
        let submit = format!("{path}{sep}wait_for_completion=false&slices=auto");
        let v = self.call_ok(Method::Post, &submit, Some(payload))?;
        let Some(task) = v["task"].as_str().map(str::to_owned) else {
            return Ok(v);
        };
        let poll = format!(
            "/_tasks/{}?wait_for_completion=true&timeout={}s",
            encode_id(&task),
            self.poll_wait.as_secs().max(1)
        );
        loop {
            let resp = self.call(Method::Get, &poll, None)?;
            // A wait that times out is not a failure: the task still runs.
            if resp.status != 408 {
                let s = ok(resp, "GET", &poll)?;
                if s["completed"].as_bool() == Some(true) {
                    if !s["error"].is_null() {
                        return Err(BackendError::Http {
                            method: "POST".into(),
                            path: path.into(),
                            status: 500,
                            reason: s["error"].to_string().chars().take(400).collect(),
                        });
                    }
                    return Ok(s["response"].clone());
                }
            }
            if started.elapsed() > self.task_budget {
                let _ = self.call(
                    Method::Post,
                    &format!("/_tasks/{}/_cancel", encode_id(&task)),
                    None,
                );
                return Err(BackendError::Transport(format!(
                    "server task {task} for {path} did not finish within {} s; it was cancelled",
                    self.task_budget.as_secs()
                )));
            }
        }
    }

    fn delete_by_query(&self, query: &Value) -> Result<u64> {
        // Delete-by-query only matches refreshed documents; make recent
        // bulk writes visible first so none survive the delete.
        self.refresh_build_indexes()?;
        let names: Vec<String> = IndexKind::BUILD_SCOPED
            .iter()
            .map(|k| self.index(*k))
            .collect();
        let path = format!(
            "/{}/_delete_by_query?conflicts=proceed&refresh=true",
            names.join(",")
        );
        let v = self.by_query(&path, &json!({ "query": query }))?;
        Ok(v["deleted"].as_u64().unwrap_or(0))
    }

    /// Removes `build_id` from every record of the source: a record that
    /// belongs to other builds too (copied forward) just loses this build
    /// id; a record that belonged only to this build is deleted. Shared
    /// records of other builds are never deleted.
    pub(crate) fn delete_build_docs(&self, source_id: &str, build_id: &str) -> Result<u64> {
        self.refresh_build_indexes()?;
        let names: Vec<String> = IndexKind::BUILD_SCOPED
            .iter()
            .map(|k| self.index(*k))
            .collect();
        let path = format!(
            "/{}/_update_by_query?conflicts=proceed&refresh=true",
            names.join(",")
        );
        let v = self.by_query(
            &path,
            &json!({
                "query": { "bool": { "filter": [
                    { "term": { "source_id": source_id } },
                    { "term": { "build_id": build_id } },
                ] } },
                "script": { "lang": "painless", "source": DETACH_SCRIPT, "params": { "b": build_id } },
            }),
        )?;
        Ok(v["deleted"].as_u64().unwrap_or(0))
    }

    /// Deletes every record of the source that belongs to none of the
    /// `keep` builds (manual links are not build-scoped and survive). Ids of
    /// dropped builds left on shared records are pruned by the next
    /// copy-forward.
    fn gc_source(&self, source_id: &str, keep: &[String]) -> Result<()> {
        self.delete_by_query(&json!({ "bool": {
            "filter": [ { "term": { "source_id": source_id } } ],
            "must_not": [
                { "terms": { "build_id": keep } },
                { "term": { "record_kind": MANUAL_LINK } },
            ],
        }}))
        .map(|_| ())
    }

    /// Deletes every record and the state of a source.
    pub fn remove_source(&self, source_id: &str) -> Result<u64> {
        let n = self.delete_by_query(&json!({ "term": { "source_id": source_id } }))?;
        self.delete_path(&format!(
            "/{}/_doc/{}?refresh=true",
            self.index(IndexKind::Runtime),
            encode_id(source_id)
        ))?;
        let path = format!(
            "/{}/_doc/{}?refresh=true",
            self.index(IndexKind::SourceState),
            encode_id(source_id)
        );
        let resp = self.call(Method::Delete, &path, None)?;
        if resp.status != 404 {
            ok(resp, "DELETE", &path)?;
        }
        Ok(n)
    }

    // ---- reads ----------------------------------------------------------

    /// `source_id -> active_build_id` for every published source, paged
    /// (no truncation). Reads take this once per request: it is the
    /// request's snapshot.
    pub fn active_builds(&self) -> Result<BTreeMap<String, String>> {
        let mut out = BTreeMap::new();
        for doc in self.scan_states(json!({ "exists": { "field": "active_build_id" } }))? {
            if let (Some(s), Some(b)) = (doc["source_id"].as_str(), doc["active_build_id"].as_str())
            {
                out.insert(s.to_owned(), b.to_owned());
            }
        }
        Ok(out)
    }

    /// Filter that only matches records of each source's active build.
    pub fn visibility_filter(active: &BTreeMap<String, String>) -> Value {
        if active.is_empty() {
            return json!({ "match_none": {} });
        }
        let should: Vec<Value> = active
            .iter()
            .map(|(s, b)| {
                json!({ "bool": { "filter": [
                    { "term": { "source_id": s } },
                    { "term": { "build_id": b } },
                ]}})
            })
            .collect();
        json!({ "bool": { "should": should, "minimum_should_match": 1 } })
    }

    fn search(
        &self,
        kind: IndexKind,
        id_field: &str,
        query: Value,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        let active = self.active_builds()?;
        let path = format!("/{}/_search", self.index(kind));
        let v = self.call_ok(
            Method::Post,
            &path,
            Some(&json!({
                "size": limit,
                "_source": [id_field, "source_id", "rel_path"],
                "query": { "bool": {
                    "must": [ query ],
                    "filter": [ Self::visibility_filter(&active) ],
                }},
            })),
        )?;
        Ok(v.pointer("/hits/hits")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|h| SearchHit {
                id: h["_source"][id_field]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                source_id: h["_source"]["source_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                rel_path: h["_source"]["rel_path"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                score: h["_score"].as_f64().unwrap_or(0.0),
            })
            .collect())
    }

    /// Basic lexical chunk search over the visible builds (ranking and
    /// fusion live in `docmonk-retrieval`).
    pub fn search_chunks(&self, text: &str, limit: usize) -> Result<Vec<SearchHit>> {
        self.search(
            IndexKind::Chunks,
            "chunk_id",
            json!({ "multi_match": { "query": text, "fields": ["search_text", "heading_path", "title", "caption"] } }),
            limit,
        )
    }

    pub fn search_code(&self, text: &str, limit: usize) -> Result<Vec<SearchHit>> {
        self.search(
            IndexKind::Code,
            "entity_id",
            json!({ "multi_match": { "query": text, "fields": ["name^3", "qualified_name^2", "signature"] } }),
            limit,
        )
    }

    /// Visible record count of one index (optionally one source).
    pub fn count(&self, kind: IndexKind, source_id: Option<&str>) -> Result<u64> {
        let active = self.active_builds()?;
        let mut filter = vec![Self::visibility_filter(&active)];
        if let Some(s) = source_id {
            filter.push(json!({ "term": { "source_id": s } }));
        }
        let v = self.call_ok(
            Method::Post,
            &format!("/{}/_count", self.index(kind)),
            Some(&json!({ "query": { "bool": { "filter": filter } } })),
        )?;
        Ok(v["count"].as_u64().unwrap_or(0))
    }

    /// `GET /_cluster/health` status (`green`/`yellow`/`red`).
    pub fn cluster_health(&self) -> Result<String> {
        let v = self.call_ok(Method::Get, "/_cluster/health", None)?;
        Ok(v["status"].as_str().unwrap_or("unknown").to_owned())
    }

    /// Raw record count including invisible builds (diagnostics/tests).
    pub fn raw_count(&self, kind: IndexKind, source_id: &str, build_id: &str) -> Result<u64> {
        let v = self.call_ok(
            Method::Post,
            &format!("/{}/_count", self.index(kind)),
            Some(&json!({ "query": { "bool": { "filter": [
                { "term": { "source_id": source_id } },
                { "term": { "build_id": build_id } },
            ]}}})),
        )?;
        Ok(v["count"].as_u64().unwrap_or(0))
    }

    // ---- build-mode tuning -------------------------------------------------

    /// Disables refresh and replicas on the build-scoped indexes for a
    /// large rebuild. Restore with [`BuildMode::restore`]. Only use while
    /// no other process serves searches from this cluster's DocMonk indexes.
    pub fn enter_build_mode(&self) -> Result<BuildMode<'_>> {
        let mut saved = Vec::new();
        for kind in IndexKind::BUILD_SCOPED {
            let name = self.index(kind);
            let v = self.call_ok(
                Method::Get,
                &format!("/{name}/_settings?include_defaults=true&flat_settings=true"),
                None,
            )?;
            let s = v
                .as_object()
                .and_then(|m| m.values().next())
                .cloned()
                .unwrap_or_default();
            let get = |k: &str| {
                s.pointer(&format!("/settings/{}", k.replace('/', "~1")))
                    .or_else(|| s.pointer(&format!("/defaults/{}", k.replace('/', "~1"))))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            };
            saved.push((
                name.clone(),
                get("index.refresh_interval").unwrap_or_else(|| "1s".into()),
                get("index.number_of_replicas")
                    .unwrap_or_else(|| self.settings.replicas.to_string()),
            ));
            self.call_ok(
                Method::Put,
                &format!("/{name}/_settings"),
                Some(&json!({ "index": { "refresh_interval": "-1", "number_of_replicas": 0 } })),
            )?;
        }
        Ok(BuildMode {
            backend: self,
            saved,
            restored: false,
        })
    }
}

/// Saved serving settings; restored explicitly or (best effort) on drop.
pub struct BuildMode<'a> {
    backend: &'a ServerBackend,
    saved: Vec<(String, String, String)>,
    restored: bool,
}

impl BuildMode<'_> {
    pub fn restore(mut self) -> Result<()> {
        self.restore_inner()
    }

    fn restore_inner(&mut self) -> Result<()> {
        if self.restored {
            return Ok(());
        }
        for (name, refresh, replicas) in &self.saved {
            self.backend.call_ok(
                Method::Put,
                &format!("/{name}/_settings"),
                Some(&json!({ "index": { "refresh_interval": refresh, "number_of_replicas": replicas } })),
            )?;
        }
        self.restored = true;
        Ok(())
    }
}

impl Drop for BuildMode<'_> {
    fn drop(&mut self) {
        let _ = self.restore_inner();
    }
}

/// Typed records written into a build. Field names match the mappings;
/// `None` fields are omitted.
#[derive(Debug, Clone, Default, Serialize)]
pub struct FileDoc {
    pub file_id: String,
    pub rel_path: String,
    pub kind: String,
    pub size: i64,
    pub mtime: f64,
    pub content_hash: Option<String>,
    pub parser_version: Option<String>,
    pub chunker_version: Option<String>,
    pub converter_version: Option<String>,
    pub embedding_model_id: Option<String>,
    pub embedding_text_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// When the file's latest error happened (epoch milliseconds), the
    /// event time the status error feed orders by.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_attempt_at: Option<String>,
    /// Digest of everything derived from this file (rows and vectors);
    /// equal digests let a later build copy the file forward server-side.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub knowledge_digest: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct EntityDoc {
    pub entity_id: String,
    pub file_id: String,
    pub rel_path: String,
    pub kind: String,
    pub name: String,
    pub qualified_name: String,
    pub language: String,
    pub parent_id: Option<String>,
    pub signature: Option<String>,
    pub start_line: i64,
    pub end_line: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_col: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_col: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtime: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DocumentDoc {
    pub document_id: String,
    pub file_id: String,
    pub rel_path: String,
    pub format: String,
    pub title: Option<String>,
    pub author: Option<String>,
    pub page_count: Option<i64>,
    pub is_scanned: bool,
    pub content_hash: Option<String>,
    pub parent_document_id: Option<String>,
    pub attachment_name: Option<String>,
    pub attachment_content_type: Option<String>,
    pub attachment_index: Option<i64>,
    pub attachment_content_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtime: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ChunkDoc {
    pub chunk_id: String,
    pub document_id: String,
    pub parent_document_id: Option<String>,
    pub file_id: String,
    pub rel_path: String,
    pub kind: String,
    pub ordinal: i64,
    pub heading_path: Vec<String>,
    pub heading_level: Option<i64>,
    /// The owning document's title (indexed for search, weight 8).
    pub title: Option<String>,
    pub search_text: String,
    pub text: String,
    pub embedding_text: Option<String>,
    pub token_count: Option<i64>,
    pub page_start: Option<i64>,
    pub page_end: Option<i64>,
    pub table_rows: Option<Vec<Vec<String>>>,
    pub caption: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_ordinal: Option<i64>,
    /// The heading text the chunk is searched under (weight 5).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fts_heading: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_index: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtime: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct EdgeDoc {
    pub relationship_id: String,
    pub file_id: String,
    pub rel_path: String,
    pub relationship_type: String,
    pub source_entity_id: String,
    pub target_entity_id: Option<String>,
    pub target_symbol: Option<String>,
    pub resolver: String,
    pub confidence: String,
    pub source_location: Option<String>,
    pub evidence: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference_text: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct LinkDoc {
    pub relationship_id: String,
    /// The link type (`mentioned_in`, `documents`, ...).
    pub relationship_type: String,
    pub entity_id: String,
    pub document_id: String,
    pub chunk_id: Option<String>,
    pub file_id: String,
    pub rel_path: String,
    pub resolver: String,
    pub confidence: String,
    pub evidence: Option<String>,
}

/// Bulk writer bound to one `(source, build)`.
pub struct BuildWriter<'a> {
    backend: &'a ServerBackend,
    source_id: String,
    build_id: String,
    bulk: BulkWriter<'a>,
}

impl BuildWriter<'_> {
    fn put(
        &mut self,
        kind: IndexKind,
        record_id: &str,
        doc: &impl Serialize,
        extra: &[(&str, &str)],
    ) -> Result<()> {
        let mut v = serde_json::to_value(doc).map_err(|e| BackendError::Invalid(e.to_string()))?;
        if let Some(obj) = v.as_object_mut() {
            obj.insert("source_id".into(), Value::String(self.source_id.clone()));
            obj.insert("build_id".into(), Value::String(self.build_id.clone()));
            for (k, val) in extra {
                obj.insert((*k).into(), Value::String((*val).into()));
            }
        }
        self.bulk.push(&Action::Index {
            index: self.backend.index(kind),
            id: format!("{}:{record_id}", self.build_id),
            doc: v,
        })
    }

    pub fn file(&mut self, d: &FileDoc) -> Result<()> {
        let path_text = d.rel_path.replace(['/', '\\', '.', '_', '-'], " ");
        self.put(
            IndexKind::Files,
            &d.file_id.clone(),
            d,
            &[("path_text", &path_text)],
        )
    }
    pub fn entity(&mut self, d: &EntityDoc) -> Result<()> {
        self.put(IndexKind::Code, &d.entity_id.clone(), d, &[])
    }
    pub fn document(&mut self, d: &DocumentDoc) -> Result<()> {
        self.put(IndexKind::Documents, &d.document_id.clone(), d, &[])
    }
    pub fn chunk(&mut self, d: &ChunkDoc) -> Result<()> {
        self.put(IndexKind::Chunks, &d.chunk_id.clone(), d, &[])
    }
    pub fn edge(&mut self, d: &EdgeDoc) -> Result<()> {
        self.put(
            IndexKind::Relationships,
            &d.relationship_id.clone(),
            d,
            &[("record_kind", "edge")],
        )
    }
    pub fn link(&mut self, d: &LinkDoc) -> Result<()> {
        self.put(
            IndexKind::Relationships,
            &d.relationship_id.clone(),
            d,
            &[("record_kind", "link")],
        )
    }

    pub fn finish(self) -> Result<BulkReport> {
        self.bulk.finish()
    }
}

/// Document IDs go into URL paths.
pub fn encode_id(id: &str) -> String {
    id.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Epoch milliseconds; both engines accept `epoch_millis` for `date` fields.
pub(crate) fn now() -> String {
    now_millis().to_string()
}

/// An RFC 3339 / naive-UTC ISO timestamp as epoch milliseconds.
pub fn iso_to_millis(v: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(v)
        .map(|d| d.timestamp_millis())
        .ok()
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(v, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|n| n.and_utc().timestamp_millis())
        })
        .map(|ms| ms.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visibility_filter_shapes() {
        assert_eq!(
            ServerBackend::visibility_filter(&BTreeMap::new()),
            json!({"match_none": {}})
        );
        let mut m = BTreeMap::new();
        m.insert("s".to_string(), "b".to_string());
        let f = ServerBackend::visibility_filter(&m);
        assert_eq!(f["bool"]["minimum_should_match"], 1);
        assert_eq!(
            f["bool"]["should"][0]["bool"]["filter"][1]["term"]["build_id"],
            "b"
        );
    }

    /// Answers a submitted by-query with a task id, times out one poll,
    /// reports it running once, then completed.
    struct Tasks(
        std::sync::Mutex<Vec<(u16, Value)>>,
        std::sync::Mutex<Vec<String>>,
    );
    impl crate::transport::Transport for Tasks {
        fn send(
            &self,
            method: Method,
            path: &str,
            _body: Option<(&[u8], &str)>,
        ) -> Result<Response> {
            self.1.lock().unwrap().push(format!("{method:?} {path}"));
            let (status, v) = self.0.lock().unwrap().remove(0);
            Ok(Response {
                status,
                body: serde_json::to_vec(&v).unwrap(),
            })
        }
    }

    #[test]
    fn by_query_runs_detached_and_polls_the_task() {
        let script = vec![
            (200, json!({"task": "n1:42"})),
            (408, json!({"error": "timeout"})),
            (200, json!({"completed": false})),
            (
                200,
                json!({"completed": true, "response": {"updated": 7, "failures": []}}),
            ),
        ];
        let calls = std::sync::Arc::new(Tasks(script.into(), Default::default()));
        struct Shared(std::sync::Arc<Tasks>);
        impl crate::transport::Transport for Shared {
            fn send(&self, m: Method, p: &str, b: Option<(&[u8], &str)>) -> Result<Response> {
                self.0.send(m, p, b)
            }
        }
        let b = ServerBackend::with_client(
            Counting::new(Box::new(Shared(calls.clone()))),
            Engine::OpenSearch,
            "t",
            None,
            BulkLimits::from_config(&docmonk_config::model::BulkConfig::default()),
        )
        .with_task_limits(Duration::from_secs(60), Duration::from_secs(2));
        let v = b
            .by_query("/t-chunks/_update_by_query?conflicts=proceed", &json!({}))
            .unwrap();
        assert_eq!(v["updated"], 7);
        let log = calls.1.lock().unwrap().clone();
        assert_eq!(
            log[0],
            "Post /t-chunks/_update_by_query?conflicts=proceed&wait_for_completion=false&slices=auto"
        );
        assert_eq!(
            log[1],
            "Get /_tasks/n1%3A42?wait_for_completion=true&timeout=2s"
        );
        assert_eq!(log.len(), 4);
    }

    #[test]
    fn by_query_cancels_a_task_past_its_budget() {
        let script = vec![
            (200, json!({"task": "n1:7"})),
            (200, json!({"completed": false})),
            (200, json!({"acknowledged": true})),
        ];
        let calls = std::sync::Arc::new(Tasks(script.into(), Default::default()));
        struct Shared(std::sync::Arc<Tasks>);
        impl crate::transport::Transport for Shared {
            fn send(&self, m: Method, p: &str, b: Option<(&[u8], &str)>) -> Result<Response> {
                self.0.send(m, p, b)
            }
        }
        let b = ServerBackend::with_client(
            Counting::new(Box::new(Shared(calls.clone()))),
            Engine::OpenSearch,
            "t",
            None,
            BulkLimits::from_config(&docmonk_config::model::BulkConfig::default()),
        )
        .with_task_limits(Duration::ZERO, Duration::from_secs(1));
        let err = b
            .by_query("/t-chunks/_delete_by_query", &json!({}))
            .unwrap_err();
        assert!(err.to_string().contains("cancelled"), "{err}");
        let log = calls.1.lock().unwrap().clone();
        assert_eq!(log[2], "Post /_tasks/n1%3A7/_cancel");
    }

    #[test]
    fn ids_are_url_safe() {
        assert_eq!(encode_id("src_ab12"), "src_ab12");
        assert_eq!(encode_id("a/b:c"), "a%2Fb%3Ac");
    }
}
