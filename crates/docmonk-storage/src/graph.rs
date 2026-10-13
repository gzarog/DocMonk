//! The relationship graph's lifecycle in one project store.
//!
//! The graph (code relationships and code-document links) is derived after
//! the base index is published and is versioned independently of it. Its
//! state lives in the knowledge database's `metadata` table under
//! [`GRAPH_STATE_KEY`], so no schema change is needed:
//!
//! * `base_build_id` + `base_generation` name the exact published base the
//!   graph rows were derived from. A base generation is the build id plus
//!   the build's `finished_at`, which changes on every publication (full
//!   or in-place incremental).
//! * Graph readers ([`ProjectStore::graph_visible`]) only return graph rows
//!   when the state is `ready` **and** its base generation equals the
//!   active base's current generation. Anything else (missing, building,
//!   failed, disabled, stale) is reported as an explicit status and graph
//!   queries come back empty; regular search is unaffected.
//! * Graph rows are written and the state is promoted to `ready` in one
//!   SQLite transaction, so a failed or crashed graph build is never
//!   visible and never touches base rows.
//! * The dependency manifest (semantic file keys, per-file symbols, the
//!   [`GraphState::input_digest`] and an explicit completeness flag) lets
//!   the graph outlive base builds: a full base rebuild carries the graph
//!   rows into the new build ([`ProjectStore::carry_graph_forward`]) and a
//!   server writer without local state adopts the published rows
//!   ([`ProjectStore::adopt_graph`]). Either way the rows stay invisible
//!   until the graph stage rebinds or incrementally updates them.

use std::collections::BTreeMap;

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::db::{now_iso, write_tx};
use crate::error::{Result, StorageError};
use crate::knowledge::{
    cached, entity_from_row, EntityRow, ProjectStore, RelationshipRow, CROSS_FILE_RESOLVERS,
    ENTITY_COLUMNS,
};

/// `metadata` key holding the JSON [`GraphState`] (small: status reads it).
pub const GRAPH_STATE_KEY: &str = "graph_state";
/// `metadata` key holding the graph's per-file dependency keys (JSON
/// `file_id -> key`), read only by the graph stage.
pub const GRAPH_FILES_KEY: &str = "graph_files";
/// `metadata` key holding the graph's per-file symbol dependencies (JSON
/// `file_id -> FileSymbols`), read only by the graph stage.
pub const GRAPH_SYMBOLS_KEY: &str = "graph_symbols";

/// Version of the dependency manifest layout ([`GraphState::files`],
/// [`GraphState::symbols`]). A manifest of another version is not reused.
pub const MANIFEST_VERSION: u32 = 2;

/// A candidate's position in cross-file resolution order within its file:
/// `(qualified name or "", start line, entity id)`.
type CandidateOrder = (String, i64, String);

/// `metadata` key holding the source's [`ExternalManifest`].
pub const GRAPH_EXTERNAL_KEY: &str = "graph_external";

/// One unresolved reference of a consumer source bound (or looked up)
/// across sources. The source-local graph row is never changed: bindings
/// are a separate layer over it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalBinding {
    pub relationship_id: String,
    pub file_id: String,
    pub reference_text: String,
    /// Producer source of the single candidate (`None` when unbound or
    /// ambiguous).
    #[serde(default)]
    pub producer: Option<String>,
    #[serde(default)]
    pub target_entity_id: Option<String>,
    /// Candidates in more than one producer (or several in one): never
    /// guessed.
    #[serde(default)]
    pub ambiguous: bool,
}

/// A producer as a consumer's bindings saw it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProducerSnapshot {
    /// The producer's base generation the bindings were computed against.
    pub base_generation: String,
    /// How the dependency is known: `declared` or `project_reference`.
    pub via: String,
}

/// Cross-source dependencies of one consumer source: its scoped producers,
/// the candidate fingerprint of every qualified lookup it made in each
/// (empty for "no candidate", so introducing a definition invalidates it)
/// and the resulting bindings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalManifest {
    /// `current`, `stale` or `pending`.
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub reason: Option<String>,
    /// Graph generation of the consumer the bindings were derived from.
    #[serde(default)]
    pub consumer_generation: u64,
    #[serde(default)]
    pub producers: BTreeMap<String, ProducerSnapshot>,
    /// `producer -> qualified key -> candidate fingerprint`.
    #[serde(default)]
    pub lookups: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default)]
    pub bindings: Vec<ExternalBinding>,
    /// References that cannot be followed automatically (package
    /// references, project references outside every registered source).
    #[serde(default)]
    pub unsupported: Vec<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// Candidate fingerprint of a qualified lookup: the candidates' entity ids
/// in resolution order (empty for none).
pub fn candidates_fingerprint(rows: &[EntityRow]) -> String {
    rows.iter()
        .map(|e| e.id.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

/// Symbol key of a qualified name.
pub fn qualified_key(qualified_name: &str) -> String {
    format!("q:{qualified_name}")
}

/// Symbol key of a bare name.
pub fn name_key(name: &str) -> String {
    format!("n:{name}")
}

/// What one file defines and what its cross-file references look up, as
/// symbol keys ([`qualified_key`], [`name_key`]). A reference is affected
/// by a change exactly when a definition under one of its keys changed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSymbols {
    /// Keys of every entity the file defines.
    #[serde(default)]
    pub defs: Vec<String>,
    /// Keys every cross-file reference of the file resolves through.
    #[serde(default)]
    pub refs: Vec<String>,
    /// Candidate fingerprint of each definition key: the file's entity ids
    /// under that key in resolution order. Cross-file resolution only sees
    /// a file's candidates through these (ids, and their order within the
    /// file), so a reference is affected by an edit exactly when the
    /// fingerprint of one of its keys changed; a body-only edit changes
    /// none.
    #[serde(default)]
    pub fps: BTreeMap<String, String>,
}

impl FileSymbols {
    /// Definition keys whose candidates differ between `old` and `new`
    /// (a missing side counts as no candidates).
    pub fn changed_keys<'a>(
        old: Option<&'a FileSymbols>,
        new: Option<&'a FileSymbols>,
    ) -> Vec<&'a str> {
        let o = old.map(|s| &s.fps);
        let n = new.map(|s| &s.fps);
        let get = |m: Option<&'a BTreeMap<String, String>>, k: &str| m.and_then(|m| m.get(k));
        o.into_iter()
            .chain(n)
            .flat_map(|m| m.keys())
            .filter(|k| get(o, k) != get(n, k))
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

/// Graph lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GraphLifecycle {
    /// `indexing.relationships_enabled: false`: no graph jobs; any retained
    /// graph rows are hidden.
    Disabled,
    /// No graph was built for this source yet.
    #[default]
    Pending,
    Building,
    Ready,
    /// The graph matches an older base generation (or its inputs changed).
    Stale,
    Failed,
}

impl GraphLifecycle {
    pub fn as_str(self) -> &'static str {
        match self {
            GraphLifecycle::Disabled => "disabled",
            GraphLifecycle::Pending => "pending",
            GraphLifecycle::Building => "building",
            GraphLifecycle::Ready => "ready",
            GraphLifecycle::Stale => "stale",
            GraphLifecycle::Failed => "failed",
        }
    }
}

/// The recorded graph state of one source.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphState {
    pub state: GraphLifecycle,
    /// Monotonic graph generation; incremented by every promotion.
    #[serde(default)]
    pub generation: u64,
    /// Base build the committed graph rows belong to.
    #[serde(default)]
    pub base_build_id: Option<String>,
    /// Base generation the committed graph rows were derived from.
    #[serde(default)]
    pub base_generation: Option<String>,
    /// Graph derivation identity (code parser + graph rules).
    #[serde(default)]
    pub derivation_version: Option<String>,
    /// Semantic digest of every graph input ([`graph_input_digest`]):
    /// two base snapshots with the same digest have the same graph.
    #[serde(default)]
    pub input_digest: Option<String>,
    /// [`MANIFEST_VERSION`] of the recorded dependency metadata.
    #[serde(default)]
    pub manifest_version: u32,
    /// The recorded dependency metadata describes every file of the
    /// committed graph (also when it has no files or no symbols: a valid
    /// empty graph is not missing metadata).
    #[serde(default)]
    pub metadata_complete: bool,
    /// The committed rows were adopted from the server without the old
    /// definitions of changed files: the next derivation re-resolves every
    /// reference once (no parsing) instead of trusting the symbol scope.
    #[serde(default)]
    pub resolve_all: bool,
    /// Set when the stored dependency metadata could not be read (a
    /// reason-coded full fallback); never stored.
    #[serde(skip)]
    pub manifest_error: Option<&'static str>,
    /// Dependency metadata: `file_id -> semantic file key` ([`GraphFile::key`])
    /// each file's graph rows were derived from. Stored under [`GRAPH_FILES_KEY`], loaded only by
    /// [`ProjectStore::graph_state_with_files`]. Empty means "recompute
    /// everything".
    #[serde(skip)]
    pub files: BTreeMap<String, String>,
    /// Dependency metadata: per-file symbol definitions and references
    /// (stored under [`GRAPH_SYMBOLS_KEY`], loaded with `files`). Empty
    /// means "re-resolve every reference".
    #[serde(skip)]
    pub symbols: BTreeMap<String, FileSymbols>,
    #[serde(default)]
    pub last_success_at: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub stale_reason: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// A graph state as reported (resolved against the active base).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GraphStatus {
    /// Effective state: a `ready` graph of an older base reports `stale`.
    pub state: GraphLifecycle,
    pub generation: u64,
    pub base_build_id: Option<String>,
    pub base_generation: Option<String>,
    pub last_success_at: Option<String>,
    pub last_error: Option<String>,
    pub stale_reason: Option<String>,
}

impl GraphState {
    /// Whether the recorded dependency metadata can be reused.
    pub fn manifest_usable(&self) -> bool {
        self.metadata_complete
            && self.manifest_version == MANIFEST_VERSION
            && self.manifest_error.is_none()
    }

    /// The effective status given the active base generation.
    pub fn status(&self, active_generation: Option<&str>) -> GraphStatus {
        let mut state = self.state;
        let mut stale_reason = self.stale_reason.clone();
        if state == GraphLifecycle::Ready && self.base_generation.as_deref() != active_generation {
            state = GraphLifecycle::Stale;
            stale_reason.get_or_insert_with(|| "the base index was republished".into());
        }
        GraphStatus {
            state,
            generation: self.generation,
            base_build_id: self.base_build_id.clone(),
            base_generation: self.base_generation.clone(),
            last_success_at: self.last_success_at.clone(),
            last_error: self.last_error.clone(),
            stale_reason,
        }
    }
}

/// `file_id -> key` of a build's files, the per-file graph dependency key.
pub type FileKeys = BTreeMap<String, String>;

/// One published file as the graph stage sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphFile {
    pub id: String,
    pub rel_path: String,
    pub kind: String,
    pub status: String,
    pub content_hash: Option<String>,
    /// Semantic key: kind, status and content hash. Write timestamps are
    /// deliberately left out, so reprocessing identical content is not a
    /// graph change (parser changes are covered by the derivation version).
    pub key: String,
}

/// The semantic key of one published file.
pub fn file_key(kind: &str, status: &str, content_hash: Option<&str>) -> String {
    format!("{kind}:{status}:{}", content_hash.unwrap_or(""))
}

/// Digest of everything a source's graph is derived from: the derivation
/// identity, every file's id and semantic key, and `extra` (link intent and
/// dependency bindings). Never covers build ids or publication times.
pub fn graph_input_digest<'a>(
    derivation: &str,
    files: impl IntoIterator<Item = (&'a str, &'a str)>,
    extra: &str,
) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(derivation.as_bytes());
    h.update([0]);
    for (id, key) in files {
        h.update(id.as_bytes());
        h.update([1]);
        h.update(key.as_bytes());
        h.update([0]);
    }
    h.update([2]);
    h.update(extra.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn invalid(e: impl std::fmt::Display) -> StorageError {
    StorageError::Invalid(e.to_string())
}

impl ProjectStore {
    /// The recorded graph state (default `pending` when none was recorded
    /// or the record is unreadable).
    pub fn graph_state(&self) -> Result<GraphState> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM metadata WHERE key = ?1",
                [GRAPH_STATE_KEY],
                |r| r.get(0),
            )
            .optional()
            .map_err(StorageError::sqlite("read graph state"))?;
        Ok(raw
            .and_then(|r| serde_json::from_str(&r).ok())
            .unwrap_or_default())
    }

    /// [`Self::graph_state`] with its per-file dependency keys.
    pub fn graph_state_with_files(&self) -> Result<GraphState> {
        let mut state = self.graph_state()?;
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM metadata WHERE key = ?1",
                [GRAPH_FILES_KEY],
                |r| r.get(0),
            )
            .optional()
            .map_err(StorageError::sqlite("read graph files"))?;
        match raw.map(|r| serde_json::from_str(&r)) {
            Some(Ok(files)) => state.files = files,
            Some(Err(_)) => state.manifest_error = Some("manifest_corrupt"),
            None => {}
        }
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM metadata WHERE key = ?1",
                [GRAPH_SYMBOLS_KEY],
                |r| r.get(0),
            )
            .optional()
            .map_err(StorageError::sqlite("read graph symbols"))?;
        match raw.map(|r| serde_json::from_str(&r)) {
            Some(Ok(symbols)) => state.symbols = symbols,
            Some(Err(_)) => state.manifest_error = Some("manifest_corrupt"),
            None => {}
        }
        if state.manifest_error.is_some() {
            state.metadata_complete = false;
        }
        Ok(state)
    }

    fn put_metadata(&mut self, key: &str, value: &str) -> Result<()> {
        write_tx(&mut self.conn, |tx| {
            cached(
                tx,
                "INSERT INTO metadata (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map_err(StorageError::sqlite("write graph state"))?;
            Ok(())
        })
    }

    /// Stores the graph state (inside the caller's session when one is
    /// open). The per-file dependency keys are left as they are.
    pub fn put_graph_state(&mut self, state: &GraphState) -> Result<()> {
        let mut state = state.clone();
        state.updated_at = Some(now_iso());
        let value = serde_json::to_string(&state).map_err(invalid)?;
        self.put_metadata(GRAPH_STATE_KEY, &value)
    }

    /// Stores the state together with its per-file dependency keys (a
    /// promotion; inside the graph transaction).
    pub fn put_graph_state_with_files(&mut self, state: &GraphState) -> Result<()> {
        let files = serde_json::to_string(&state.files).map_err(invalid)?;
        self.put_metadata(GRAPH_FILES_KEY, &files)?;
        let symbols = serde_json::to_string(&state.symbols).map_err(invalid)?;
        self.put_metadata(GRAPH_SYMBOLS_KEY, &symbols)?;
        self.put_graph_state(state)
    }

    /// Updates only the lifecycle fields of the stored state, keeping the
    /// committed generation and dependency metadata.
    pub fn set_graph_lifecycle(
        &mut self,
        state: GraphLifecycle,
        error: Option<&str>,
        stale_reason: Option<&str>,
    ) -> Result<GraphState> {
        let mut s = self.graph_state()?;
        s.state = state;
        if error.is_some() || state != GraphLifecycle::Failed {
            s.last_error = error.map(str::to_owned);
        }
        s.stale_reason = stale_reason.map(str::to_owned);
        self.put_graph_state(&s)?;
        Ok(s)
    }

    /// The current generation of a published base build:
    /// `<build_id>@<finished_at>`. `None` when the build is not published.
    pub fn base_generation(&self, build_id: &str) -> Result<Option<String>> {
        let finished: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT finished_at FROM builds WHERE id = ?1 AND status = 'published'",
                [build_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(StorageError::sqlite("base generation"))?;
        Ok(finished.map(|f| format!("{build_id}@{}", f.unwrap_or_default())))
    }

    /// Whether graph rows of `build_id` may be returned to readers: the
    /// graph is `ready` and was derived from the build's current
    /// generation.
    pub fn graph_visible(&self, build_id: &str) -> Result<bool> {
        let state = self.graph_state()?;
        if state.state != GraphLifecycle::Ready {
            return Ok(false);
        }
        Ok(state.base_generation.is_some()
            && state.base_generation == self.base_generation(build_id)?)
    }

    /// The effective graph status against `build_id` (the active base).
    pub fn graph_status(&self, build_id: Option<&str>) -> Result<GraphStatus> {
        let generation = match build_id {
            Some(b) => self.base_generation(b)?,
            None => None,
        };
        Ok(self.graph_state()?.status(generation.as_deref()))
    }

    /// Every file of a published build with its semantic graph key.
    pub fn graph_files(&self, build_id: &str) -> Result<Vec<GraphFile>> {
        self.query_rows(
            "graph files",
            "SELECT id, rel_path, kind, status, content_hash
             FROM files WHERE build_id = ?1 ORDER BY id",
            &[&build_id],
            |r| {
                let kind: String = r.get(2)?;
                let status: String = r.get(3)?;
                let hash: Option<String> = r.get(4)?;
                Ok(GraphFile {
                    id: r.get(0)?,
                    rel_path: r.get(1)?,
                    key: file_key(&kind, &status, hash.as_deref()),
                    kind,
                    status,
                    content_hash: hash,
                })
            },
        )
    }

    /// The current [`FileSymbols`] of `file_ids` in `build_id` (every file
    /// with entities or cross-file references when `None`).
    pub fn file_symbols(
        &self,
        build_id: &str,
        file_ids: Option<&[String]>,
    ) -> Result<BTreeMap<String, FileSymbols>> {
        let mut out: BTreeMap<String, FileSymbols> = BTreeMap::new();
        let add_defs = |rows: Vec<EntityRow>, out: &mut BTreeMap<String, FileSymbols>| {
            // Per file and key, the candidates in cross-file resolution
            // order: qualified lookups sort by line, bare-name lookups by
            // qualified name (then line, id).
            let mut by_key: BTreeMap<(String, String), Vec<CandidateOrder>> = BTreeMap::new();
            for e in rows {
                let q = qualified_key(&e.qualified_name);
                let n = name_key(&e.name);
                by_key.entry((e.file_id.clone(), q)).or_default().push((
                    String::new(),
                    e.start_line,
                    e.id.clone(),
                ));
                by_key.entry((e.file_id.clone(), n)).or_default().push((
                    e.qualified_name.clone(),
                    e.start_line,
                    e.id.clone(),
                ));
            }
            for ((file, key), mut c) in by_key {
                c.sort();
                let fp = c
                    .iter()
                    .map(|(_, _, id)| id.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                let s = out.entry(file).or_default();
                s.defs.push(key.clone());
                s.fps.insert(key, fp);
            }
        };
        let add_refs = |rows: Vec<RelationshipRow>, out: &mut BTreeMap<String, FileSymbols>| {
            for r in rows {
                let Some(text) = r.reference_text.as_deref() else {
                    continue;
                };
                if !CROSS_FILE_RESOLVERS.contains(&r.resolver.as_str()) {
                    continue;
                }
                let s = out.entry(r.file_id).or_default();
                s.refs.push(qualified_key(text));
                s.refs
                    .push(name_key(text.rsplit('.').next().unwrap_or(text)));
            }
        };
        match file_ids {
            None => {
                add_defs(self.all_entities(build_id)?, &mut out);
                add_refs(self.cross_file_references(build_id)?, &mut out);
            }
            Some(ids) => {
                for id in ids {
                    out.entry(id.clone()).or_default();
                    add_defs(self.file_entities(build_id, id)?, &mut out);
                    add_refs(self.file_relationships(build_id, id)?, &mut out);
                }
            }
        }
        for s in out.values_mut() {
            s.defs.sort();
            s.defs.dedup();
            s.refs.sort();
            s.refs.dedup();
        }
        Ok(out)
    }

    /// Entities whose qualified name is `qn`, in cross-file resolution
    /// order (file, line, id).
    pub fn entities_by_qualified_name(&self, build_id: &str, qn: &str) -> Result<Vec<EntityRow>> {
        self.query_rows(
            "entities by qualified name",
            &format!(
                "SELECT {ENTITY_COLUMNS} FROM entities WHERE build_id = ?1 AND qualified_name = ?2
                 ORDER BY file_id, start_line, id"
            ),
            &[&build_id, &qn],
            entity_from_row,
        )
    }

    /// Entities whose bare name is `name`, in bare-name resolution order
    /// (qualified name, file, line, id).
    pub fn entities_by_bare_name(&self, build_id: &str, name: &str) -> Result<Vec<EntityRow>> {
        self.query_rows(
            "entities by name",
            &format!(
                "SELECT {ENTITY_COLUMNS} FROM entities WHERE build_id = ?1 AND name = ?2
                 ORDER BY qualified_name, file_id, start_line, id"
            ),
            &[&build_id, &name],
            entity_from_row,
        )
    }

    /// Cross-file references recorded by `file_id`.
    pub fn file_cross_file_references(
        &self,
        build_id: &str,
        file_id: &str,
    ) -> Result<Vec<RelationshipRow>> {
        Ok(self
            .file_relationships(build_id, file_id)?
            .into_iter()
            .filter(|r| {
                r.reference_text.is_some() && CROSS_FILE_RESOLVERS.contains(&r.resolver.as_str())
            })
            .collect())
    }

    /// Deletes every graph row (relationships and automatic/manual link
    /// projections) of `build_id`. Base rows are never touched.
    pub fn clear_graph(&mut self, build_id: &str) -> Result<()> {
        write_tx(&mut self.conn, |tx| {
            for sql in [
                "DELETE FROM relationships WHERE build_id = ?1",
                "DELETE FROM cross_links WHERE build_id = ?1",
            ] {
                cached(tx, sql, [build_id]).map_err(StorageError::sqlite("clear graph"))?;
            }
            Ok(())
        })
    }

    /// Deletes the graph rows derived from (or pointing into) one file.
    pub fn clear_file_graph(&mut self, build_id: &str, file_id: &str) -> Result<()> {
        write_tx(&mut self.conn, |tx| {
            for sql in [
                "DELETE FROM relationships WHERE build_id = ?1 AND file_id = ?2",
                "DELETE FROM cross_links WHERE build_id = ?1 AND document_id IN
                    (SELECT id FROM documents WHERE build_id = ?1 AND file_id = ?2)",
                "DELETE FROM cross_links WHERE build_id = ?1 AND entity_id IN
                    (SELECT id FROM entities WHERE build_id = ?1 AND file_id = ?2)",
            ] {
                cached(tx, sql, params![build_id, file_id])
                    .map_err(StorageError::sqlite("clear file graph"))?;
            }
            Ok(())
        })
    }

    /// Carries the committed graph of `from` (the build being superseded)
    /// into the newly published build `to`, before `from` is collected:
    /// relationships of files that still exist and links whose entity and
    /// document still exist (ids are content-addressed, so they are
    /// identical across builds). The graph state then names `to` as the
    /// build holding its rows but keeps its old base generation, so nothing
    /// becomes visible until the graph stage validated the new snapshot
    /// (rebind, or an incremental update of the changed files against the
    /// dependency manifest). Returns `false` when `from` holds no graph.
    pub fn carry_graph_forward(&mut self, from: &str, to: &str) -> Result<bool> {
        let mut state = self.graph_state()?;
        if state.base_build_id.as_deref() != Some(from) || from == to {
            return Ok(false);
        }
        write_tx(&mut self.conn, |tx| {
            for sql in [
                "INSERT OR IGNORE INTO relationships (id, build_id, file_id, relationship_type,
                    source_entity_id, target_entity_id, target_symbol, resolver, confidence,
                    source_location, evidence, reference_text)
                 SELECT r.id, ?2, r.file_id, r.relationship_type, r.source_entity_id,
                    r.target_entity_id, r.target_symbol, r.resolver, r.confidence,
                    r.source_location, r.evidence, r.reference_text
                 FROM relationships r
                 WHERE r.build_id = ?1
                   AND r.file_id IN (SELECT id FROM files WHERE build_id = ?2)",
                "INSERT OR IGNORE INTO cross_links (id, build_id, link_type, entity_id,
                    document_id, chunk_id, resolver, confidence, evidence)
                 SELECT l.id, ?2, l.link_type, l.entity_id, l.document_id, l.chunk_id,
                    l.resolver, l.confidence, l.evidence
                 FROM cross_links l
                 WHERE l.build_id = ?1
                   AND l.entity_id IN (SELECT id FROM entities WHERE build_id = ?2)
                   AND l.document_id IN (SELECT id FROM documents WHERE build_id = ?2)",
            ] {
                cached(tx, sql, params![from, to])
                    .map_err(StorageError::sqlite("carry graph forward"))?;
            }
            Ok(())
        })?;
        state.base_build_id = Some(to.to_owned());
        self.put_graph_state(&state)?;
        Ok(true)
    }

    /// Adopts graph rows published elsewhere (the server's visible
    /// generation) into `build_id`, whose own graph is empty: rows of files
    /// absent from the build and links to absent entities or documents are
    /// dropped. `files` is the manifest the rows were derived from. Like
    /// [`Self::carry_graph_forward`], the result is not visible until the
    /// graph stage validated it; the symbol scope is rebuilt from the rows
    /// and, because the old definitions of changed files are unknown, the
    /// next derivation re-resolves every reference ([`GraphState::resolve_all`]).
    pub fn adopt_graph(
        &mut self,
        build_id: &str,
        edges: &[RelationshipRow],
        links: &[crate::knowledge::LinkRow],
        files: FileKeys,
        derivation: &str,
        input_digest: Option<&str>,
    ) -> Result<()> {
        let present: std::collections::HashSet<String> =
            self.files(build_id)?.into_iter().map(|f| f.id).collect();
        let entities: std::collections::HashSet<String> = self
            .all_entities(build_id)?
            .into_iter()
            .map(|e| e.id)
            .collect();
        let docs: std::collections::HashSet<String> = self
            .documents(build_id)?
            .into_iter()
            .map(|d| d.id)
            .collect();
        self.clear_graph(build_id)?;
        let edges: Vec<RelationshipRow> = edges
            .iter()
            .filter(|r| present.contains(&r.file_id))
            .cloned()
            .collect();
        self.put_relationships(build_id, &edges)?;
        let links: Vec<crate::knowledge::LinkRow> = links
            .iter()
            .filter(|l| entities.contains(&l.entity_id) && docs.contains(&l.document_id))
            .cloned()
            .collect();
        self.put_links(build_id, &links)?;
        let mut state = self.graph_state()?;
        state.base_build_id = Some(build_id.to_owned());
        state.base_generation = None;
        state.derivation_version = Some(derivation.to_owned());
        state.input_digest = input_digest.map(str::to_owned);
        state.files = files;
        state.symbols = self.file_symbols(build_id, None)?;
        state.manifest_version = MANIFEST_VERSION;
        state.metadata_complete = true;
        state.resolve_all = true;
        self.put_graph_state_with_files(&state)
    }

    /// Relative paths of the build's files ending in `suffix` (one indexed
    /// query; dependency discovery never lists the whole build).
    pub fn file_paths_with_suffix(&self, build_id: &str, suffix: &str) -> Result<Vec<String>> {
        let pattern = format!("%{}", suffix.replace('%', "").replace('_', "\\_"));
        self.query_rows(
            "files by suffix",
            "SELECT rel_path FROM files WHERE build_id = ?1 AND rel_path LIKE ?2 ESCAPE '\\'
             ORDER BY rel_path",
            &[&build_id, &pattern],
            |r| r.get(0),
        )
    }

    /// The source's cross-source dependency manifest (`None` when it never
    /// had one or the record is unreadable).
    pub fn external_manifest(&self) -> Result<Option<ExternalManifest>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM metadata WHERE key = ?1",
                [GRAPH_EXTERNAL_KEY],
                |r| r.get(0),
            )
            .optional()
            .map_err(StorageError::sqlite("read external manifest"))?;
        Ok(raw.and_then(|r| serde_json::from_str(&r).ok()))
    }

    pub fn put_external_manifest(&mut self, m: &ExternalManifest) -> Result<()> {
        let mut m = m.clone();
        m.updated_at = Some(now_iso());
        let value = serde_json::to_string(&m).map_err(invalid)?;
        self.put_metadata(GRAPH_EXTERNAL_KEY, &value)
    }

    /// Revision of the manual link intent (user data): a digest of every
    /// manual link, part of the graph input digest so an edited intent is
    /// re-applied even when no indexed content changed.
    pub fn manual_link_revision(&self) -> Result<String> {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        for m in self.manual_links()? {
            h.update(
                format!(
                    "{}|{}|{}|{}|{:?}|{:?}\n",
                    m.id,
                    m.link_type,
                    m.entity_qualified_name,
                    m.document_rel_path,
                    m.attachment_index,
                    m.chunk_ordinal
                )
                .as_bytes(),
            );
        }
        Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
    }

    /// Inserts relationship rows into `build_id`.
    pub fn put_relationships(&mut self, build_id: &str, rows: &[RelationshipRow]) -> Result<()> {
        write_tx(&mut self.conn, |tx| {
            for r in rows {
                cached(
                    tx,
                    "INSERT OR REPLACE INTO relationships (id, build_id, file_id, relationship_type,
                        source_entity_id, target_entity_id, target_symbol, resolver, confidence,
                        source_location, evidence, reference_text)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                    params![
                        r.id,
                        build_id,
                        r.file_id,
                        r.relationship_type,
                        r.source_entity_id,
                        r.target_entity_id,
                        r.target_symbol,
                        r.resolver,
                        r.confidence,
                        r.source_location,
                        r.evidence,
                        r.reference_text
                    ],
                )
                .map_err(StorageError::sqlite("insert relationship"))?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_graph_of_an_older_base_reports_stale() {
        let s = GraphState {
            state: GraphLifecycle::Ready,
            base_generation: Some("b@1".into()),
            ..GraphState::default()
        };
        assert_eq!(s.status(Some("b@1")).state, GraphLifecycle::Ready);
        let st = s.status(Some("b@2"));
        assert_eq!(st.state, GraphLifecycle::Stale);
        assert!(st.stale_reason.is_some());
        assert_eq!(s.status(None).state, GraphLifecycle::Stale);
    }

    #[test]
    fn file_keys_are_not_part_of_the_state_record() {
        let s = GraphState {
            files: [("f".to_owned(), "k".to_owned())].into(),
            ..GraphState::default()
        };
        assert!(!serde_json::to_string(&s).unwrap().contains("\"f\""));
    }

    #[test]
    fn file_keys_ignore_write_times_and_digest_ignores_snapshot_ids() {
        assert_eq!(file_key("code", "indexed", Some("h")), "code:indexed:h");
        let a = graph_input_digest("d", [("f", "code:indexed:h")], "");
        assert_eq!(a, graph_input_digest("d", [("f", "code:indexed:h")], ""));
        assert_ne!(a, graph_input_digest("d", [("f", "code:indexed:x")], ""));
        assert_ne!(a, graph_input_digest("d2", [("f", "code:indexed:h")], ""));
        assert_ne!(a, graph_input_digest("d", [("f", "code:indexed:h")], "m"));
    }

    #[test]
    fn a_corrupt_manifest_is_reported_not_mistaken_for_an_empty_one() {
        let d = tempfile::tempdir().unwrap();
        let mut s = ProjectStore::open(&crate::StorageLayout::at(d.path()), "p", "s", 8).unwrap();
        let ok = GraphState {
            manifest_version: MANIFEST_VERSION,
            metadata_complete: true,
            ..GraphState::default()
        };
        s.put_graph_state_with_files(&ok).unwrap();
        let read = s.graph_state_with_files().unwrap();
        assert!(read.manifest_usable(), "a valid empty manifest is usable");
        s.put_metadata(GRAPH_SYMBOLS_KEY, "{not json").unwrap();
        let read = s.graph_state_with_files().unwrap();
        assert_eq!(read.manifest_error, Some("manifest_corrupt"));
        assert!(!read.manifest_usable());
    }

    #[test]
    fn only_candidate_changes_count_as_changed_keys() {
        let a = FileSymbols {
            fps: [
                ("q:A.f".to_owned(), "e1".to_owned()),
                ("n:f".to_owned(), "e1".to_owned()),
            ]
            .into(),
            ..FileSymbols::default()
        };
        assert!(FileSymbols::changed_keys(Some(&a), Some(&a)).is_empty());
        let mut b = a.clone();
        b.fps.insert("n:f".into(), "e1,e2".into());
        assert_eq!(FileSymbols::changed_keys(Some(&a), Some(&b)), vec!["n:f"]);
        let mut removed = FileSymbols::changed_keys(Some(&a), None);
        removed.sort();
        assert_eq!(removed, vec!["n:f", "q:A.f"]);
    }

    #[test]
    fn unknown_fields_default() {
        let s: GraphState = serde_json::from_str(r#"{"state":"failed"}"#).unwrap();
        assert_eq!(s.state, GraphLifecycle::Failed);
        assert!(s.files.is_empty());
    }
}
