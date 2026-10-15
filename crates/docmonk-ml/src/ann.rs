//! Persistent ANN index per project.
//!
//! SQLite stays authoritative. The index file (`<project>/ann/index.hnsw`)
//! is a cache of the active build's vectors:
//!
//! * The file is written atomically (temp file + rename). A header carries
//!   the format version, model fingerprint, build id and dimensions, and a
//!   trailing sha256 covers the whole payload. A missing, corrupt, truncated
//!   or mismatched file is discarded and rebuilt, never trusted.
//! * `sync` diffs the index's labels against the build's vectors in SQLite.
//!   Missing ones are added and extra ones tombstoned. It compacts when
//!   tombstones exceed `COMPACT_RATIO`, and rebuilds when the model changed.
//! * Incremental syncs append to a delta log (`index.hnsw.delta`) instead
//!   of rewriting the file: one record per sync holds that sync's removes
//!   and inserts plus the header it leaves behind. Index operations are
//!   deterministic, so replaying the log onto the base reproduces the
//!   synced graph exactly. The log starts with the base file's checksum;
//!   each record carries its own sha256, and replay stops at the first
//!   torn or corrupt record (the header then names older content, so
//!   queries fall back to exact search and the next sync rewrites the
//!   file). When the log outgrows `DELTA_COMPACT_RATIO` of the base, the
//!   whole index is rewritten and the log removed.
//! * The header's `content` is the build's embeddings revision token
//!   ([`ProjectStore::embeddings_revision`]), a random value SQLite
//!   replaces in the same transaction as every insert or delete of the
//!   build's vectors. Queries use the index only when its header names the
//!   requested build and model and its token still equals SQLite's (one
//!   point read). Otherwise they fall back to exact search over SQLite,
//!   which is always correct.
//! * A process keeps the last valid index per project directory in memory
//!   ([`search`]), so a long-lived process (MCP, UI, daemon) reads and
//!   decodes the file once per revision, not once per query.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use docmonk_storage::knowledge::ProjectStore;
use sha2::{Digest, Sha256};

use crate::hnsw::{exact_top_k, Hnsw, HnswParams};

const MAGIC: &[u8; 8] = b"RMHNSW\0\x01";
/// Bump on any change to the on-disk layout.
pub const FORMAT_VERSION: u32 = 1;
/// Tombstone share above which `sync` compacts the graph.
pub const COMPACT_RATIO: f64 = 0.25;
const DELTA_MAGIC: &[u8; 8] = b"RMHNSWD\x01";
/// Delta log size, as a share of the base file, above which `sync`
/// rewrites the base instead of appending (bounds replay work on load).
pub const DELTA_COMPACT_RATIO: f64 = 0.25;

/// Header of a persisted index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexMeta {
    pub format_version: u32,
    pub fingerprint: String,
    pub build_id: String,
    pub dims: usize,
    /// The build's embeddings revision token the index mirrors.
    pub content: String,
}

/// `<project>/ann/index.hnsw`.
pub fn index_path(project_dir: &Path) -> PathBuf {
    project_dir.join("ann").join("index.hnsw")
}

/// The delta log next to `index` (`index.hnsw.delta`).
pub fn delta_path(index: &Path) -> PathBuf {
    let mut name = index.file_name().unwrap_or_default().to_owned();
    name.push(".delta");
    index.with_file_name(name)
}

/// Label used in the index for a stored vector.
pub fn label(subject_type: &str, subject_id: &str) -> String {
    format!("{subject_type}:{subject_id}")
}

/// Splits a label back into `(subject_type, subject_id)`.
pub fn split_label(label: &str) -> Option<(&str, &str)> {
    label.split_once(':')
}

struct Writer(Vec<u8>);

impl Writer {
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn str(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.0.extend_from_slice(s.as_bytes());
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let out = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(out)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn len(&mut self) -> Option<usize> {
        usize::try_from(self.u32()?).ok()
    }
    fn str(&mut self) -> Option<String> {
        let n = self.len()?;
        String::from_utf8(self.take(n)?.to_vec()).ok()
    }
}

/// Serializes `index` with `meta` (deterministic bytes).
pub fn encode(index: &Hnsw, meta: &IndexMeta) -> Vec<u8> {
    let mut w = Writer(Vec::new());
    w.0.extend_from_slice(MAGIC);
    w.u32(meta.format_version);
    w.str(&meta.fingerprint);
    w.str(&meta.build_id);
    w.str(&meta.content);
    w.u32(index.dims as u32);
    let p = index.params;
    w.u32(p.m as u32);
    w.u32(p.ef_construction as u32);
    w.u32(p.ef_search as u32);
    w.u64(p.seed);
    w.u64(index.rng);
    w.u32(index.entry.map_or(u32::MAX, |e| e));
    w.u32(index.labels.len() as u32);
    for i in 0..index.labels.len() {
        w.str(&index.labels[i]);
        w.0.push(u8::from(index.deleted[i]));
        for x in &index.vectors[i * index.dims..(i + 1) * index.dims] {
            w.0.extend_from_slice(&x.to_le_bytes());
        }
        w.u32(index.links[i].len() as u32);
        for layer in &index.links[i] {
            w.u32(layer.len() as u32);
            for n in layer {
                w.u32(*n);
            }
        }
    }
    let digest = Sha256::digest(&w.0);
    w.0.extend_from_slice(&digest);
    w.0
}

/// Parses bytes written by [`encode`]; `None` for anything malformed,
/// truncated, of another format version or failing its checksum.
pub fn decode(bytes: &[u8]) -> Option<(Hnsw, IndexMeta)> {
    let body_len = bytes.len().checked_sub(32)?;
    let (body, digest) = bytes.split_at(body_len);
    if Sha256::digest(body).as_slice() != digest {
        return None;
    }
    let mut r = Reader { buf: body, pos: 0 };
    if r.take(8)? != MAGIC {
        return None;
    }
    let format_version = r.u32()?;
    if format_version != FORMAT_VERSION {
        return None;
    }
    let fingerprint = r.str()?;
    let build_id = r.str()?;
    let content = r.str()?;
    let dims = r.len()?;
    let params = HnswParams {
        m: r.len()?,
        ef_construction: r.len()?,
        ef_search: r.len()?,
        seed: r.u64()?,
    };
    let rng = r.u64()?;
    let entry = match r.u32()? {
        u32::MAX => None,
        e => Some(e),
    };
    let count = r.len()?;
    let mut labels = Vec::with_capacity(count.min(1 << 20));
    let mut deleted = Vec::with_capacity(count.min(1 << 20));
    let mut vectors = Vec::with_capacity(count.min(1 << 20) * dims);
    let mut links = Vec::with_capacity(count.min(1 << 20));
    for _ in 0..count {
        labels.push(r.str()?);
        deleted.push(r.take(1)?[0] != 0);
        for c in r.take(dims.checked_mul(4)?)?.chunks_exact(4) {
            vectors.push(f32::from_le_bytes([c[0], c[1], c[2], c[3]]));
        }
        let layers = r.len()?;
        let mut node = Vec::with_capacity(layers.min(32));
        for _ in 0..layers {
            let n = r.len()?;
            let mut list = Vec::with_capacity(n.min(1024));
            for _ in 0..n {
                let to = r.u32()?;
                if to as usize >= count {
                    return None;
                }
                list.push(to);
            }
            node.push(list);
        }
        links.push(node);
    }
    if r.pos != body.len() || entry.is_some_and(|e| e as usize >= count) {
        return None;
    }
    let meta = IndexMeta {
        format_version,
        fingerprint,
        build_id,
        dims,
        content,
    };
    Some((
        Hnsw::from_parts(params, dims, vectors, labels, links, deleted, entry, rng),
        meta,
    ))
}

/// One change replayed from the delta log.
#[derive(Debug, Clone, PartialEq)]
pub enum DeltaOp {
    Remove(String),
    Insert(String, Vec<f32>),
}

fn encode_record(build_id: &str, content: &str, dims: usize, ops: &[DeltaOp]) -> Vec<u8> {
    let mut body = Writer(Vec::new());
    body.str(build_id);
    body.str(content);
    body.u32(ops.len() as u32);
    for op in ops {
        match op {
            DeltaOp::Remove(label) => {
                body.0.push(0);
                body.str(label);
            }
            DeltaOp::Insert(label, v) => {
                body.0.push(1);
                body.str(label);
                debug_assert_eq!(v.len(), dims);
                for x in v {
                    body.0.extend_from_slice(&x.to_le_bytes());
                }
            }
        }
    }
    let mut out = Writer(Vec::with_capacity(body.0.len() + 36));
    out.u32(body.0.len() as u32);
    out.0.extend_from_slice(&body.0);
    out.0.extend_from_slice(&Sha256::digest(&body.0));
    out.0
}

/// Parses one record body: `(build_id, content, ops)`.
fn decode_record(body: &[u8], dims: usize) -> Option<(String, String, Vec<DeltaOp>)> {
    let mut r = Reader { buf: body, pos: 0 };
    let build_id = r.str()?;
    let content = r.str()?;
    let n = r.len()?;
    let mut ops = Vec::with_capacity(n.min(1 << 16));
    for _ in 0..n {
        let tag = r.take(1)?[0];
        let label = r.str()?;
        match tag {
            0 => ops.push(DeltaOp::Remove(label)),
            1 => {
                let v = r
                    .take(dims.checked_mul(4)?)?
                    .chunks_exact(4)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
                ops.push(DeltaOp::Insert(label, v));
            }
            _ => return None,
        }
    }
    (r.pos == body.len()).then_some((build_id, content, ops))
}

fn apply(index: &mut Hnsw, ops: &[DeltaOp]) {
    for op in ops {
        match op {
            DeltaOp::Remove(label) => {
                index.remove(label);
            }
            DeltaOp::Insert(label, v) => index.insert(label, v),
        }
    }
}

/// Appends one record (creating the log, headed by the base file's
/// checksum, when absent) and fsyncs it. Returns the bytes written.
pub fn append_delta(
    path: &Path,
    base_digest: &[u8],
    meta: &IndexMeta,
    ops: &[DeltaOp],
) -> std::io::Result<u64> {
    let delta = delta_path(path);
    let mut bytes = Vec::new();
    if std::fs::metadata(&delta).map_or(true, |m| m.len() == 0) {
        bytes.extend_from_slice(DELTA_MAGIC);
        bytes.extend_from_slice(base_digest);
    }
    bytes.extend(encode_record(&meta.build_id, &meta.content, meta.dims, ops));
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&delta)?;
    f.write_all(&bytes)?;
    f.sync_all()?;
    Ok(bytes.len() as u64)
}

/// A valid base file with its delta log replayed.
pub struct Loaded {
    pub index: Hnsw,
    pub meta: IndexMeta,
    /// The base file's trailing sha256.
    pub base_digest: Vec<u8>,
    pub base_bytes: u64,
    pub delta_bytes: u64,
    /// The whole log (if any) was replayed: it can be appended to.
    pub delta_clean: bool,
}

/// Loads the base file and replays its delta log.
pub fn load_full(path: &Path) -> Option<Loaded> {
    let bytes = std::fs::read(path).ok()?;
    crate::counters::ann_loaded();
    let (index, meta) = decode(&bytes)?;
    let mut loaded = Loaded {
        index,
        meta,
        base_digest: bytes[bytes.len() - 32..].to_vec(),
        base_bytes: bytes.len() as u64,
        delta_bytes: 0,
        delta_clean: true,
    };
    if let Ok(log) = std::fs::read(delta_path(path)) {
        loaded.delta_bytes = log.len() as u64;
        loaded.delta_clean = replay(&mut loaded, &log);
    }
    Some(loaded)
}

/// Applies every valid record of `log`; false when it is not this base's
/// log or a record is torn or corrupt (replay stops there).
fn replay(loaded: &mut Loaded, log: &[u8]) -> bool {
    if log.len() < 40 || &log[..8] != DELTA_MAGIC || log[8..40] != loaded.base_digest[..] {
        return log.is_empty();
    }
    let mut pos = 40;
    while pos < log.len() {
        let Some(len) = log
            .get(pos..pos + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
        else {
            return false;
        };
        let Some(body) = log.get(pos + 4..pos + 4 + len) else {
            return false;
        };
        let Some(digest) = log.get(pos + 4 + len..pos + 4 + len + 32) else {
            return false;
        };
        if Sha256::digest(body).as_slice() != digest {
            return false;
        }
        let Some((build_id, content, ops)) = decode_record(body, loaded.meta.dims) else {
            return false;
        };
        apply(&mut loaded.index, &ops);
        loaded.meta.build_id = build_id;
        loaded.meta.content = content;
        pos += 4 + len + 32;
    }
    true
}

/// Writes atomically: temp file in the same directory, fsync, rename.
/// Removes the delta log, which belonged to the replaced base.
pub fn save(path: &Path, index: &Hnsw, meta: &IndexMeta) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("index"),
        std::process::id()
    ));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&encode(index, meta))?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    match std::fs::remove_file(delta_path(path)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Loads a valid index file with its delta log replayed; `None` when
/// missing or invalid.
pub fn load(path: &Path) -> Option<(Hnsw, IndexMeta)> {
    load_full(path).map(|l| (l.index, l.meta))
}

/// What a `sync` did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncStats {
    pub rebuilt: bool,
    pub added: usize,
    pub removed: usize,
    pub compacted: bool,
    pub live: usize,
    /// The changes were appended to the delta log (no full rewrite).
    pub appended: bool,
    /// Bytes this sync wrote to disk.
    pub written_bytes: u64,
}

/// Brings the project's index in line with `build_id`'s vectors under
/// `fingerprint` and persists it.
pub fn sync(
    store: &ProjectStore,
    project_dir: &Path,
    build_id: &str,
    fingerprint: &str,
    dims: usize,
) -> Result<SyncStats, String> {
    let path = index_path(project_dir);
    let mut stats = SyncStats::default();
    let content = store
        .embeddings_revision(build_id)
        .map_err(|e| e.to_string())?;
    let (mut index, base) = match load_full(&path) {
        Some(l) if l.meta.fingerprint == fingerprint && l.meta.dims == dims => {
            if l.meta.build_id == build_id && l.meta.content == content {
                stats.live = l.index.len();
                return Ok(stats);
            }
            let appendable = l.delta_clean;
            let base = appendable.then_some((l.base_digest, l.base_bytes, l.delta_bytes));
            (l.index, base)
        }
        _ => {
            stats.rebuilt = true;
            (Hnsw::new(dims, HnswParams::default()), None)
        }
    };
    let mut ops: Vec<DeltaOp> = Vec::new();
    let keys = store
        .embedding_keys(build_id, fingerprint)
        .map_err(|e| e.to_string())?;
    let wanted: HashSet<String> = keys.iter().map(|(t, id)| label(t, id)).collect();
    let stale: Vec<String> = index
        .labels()
        .filter(|l| !wanted.contains(*l))
        .map(str::to_owned)
        .collect();
    for l in &stale {
        index.remove(l);
        ops.push(DeltaOp::Remove(l.clone()));
    }
    stats.removed = stale.len();
    let missing: Vec<(String, String)> = keys
        .into_iter()
        .filter(|(t, id)| !index.contains(&label(t, id)))
        .collect();
    for chunk in missing.chunks(1024) {
        for (t, id, v) in store
            .embedding_vectors(build_id, fingerprint, chunk)
            .map_err(|e| e.to_string())?
        {
            if v.len() == dims {
                let l = label(&t, &id);
                index.insert(&l, &v);
                ops.push(DeltaOp::Insert(l, v));
                stats.added += 1;
            }
        }
    }
    if index.tombstone_ratio() > COMPACT_RATIO {
        index = index.compacted();
        stats.compacted = true;
    }
    stats.live = index.len();
    let meta = IndexMeta {
        format_version: FORMAT_VERSION,
        fingerprint: fingerprint.to_owned(),
        build_id: build_id.to_owned(),
        dims,
        content,
    };
    if let (false, Some((digest, base_bytes, delta_bytes))) = (stats.compacted, &base) {
        let record = encode_record(&meta.build_id, &meta.content, dims, &ops).len() as u64;
        if ((delta_bytes + record) as f64) <= DELTA_COMPACT_RATIO * *base_bytes as f64 {
            stats.written_bytes = append_delta(&path, digest, &meta, &ops)
                .map_err(|e| format!("append {}: {e}", delta_path(&path).display()))?;
            stats.appended = true;
            return Ok(stats);
        }
    }
    save(&path, &index, &meta).map_err(|e| format!("write {}: {e}", path.display()))?;
    stats.written_bytes = std::fs::metadata(&path).map_or(0, |m| m.len());
    Ok(stats)
}

/// `(subject_type, subject_id, similarity)`.
pub type AnnHit = (String, String, f32);

/// How a query was answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Hnsw,
    Exact,
    /// The server backend's own k-NN (OpenSearch/Elasticsearch).
    Server,
}

/// Where one ANN query spent its time (milliseconds).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AnnTimings {
    /// Reading, verifying and decoding the index file.
    pub load_ms: f64,
    /// Confirming the index still mirrors the build's vectors.
    pub freshness_ms: f64,
    /// The graph search (or the exact scan when the index was unusable).
    pub search_ms: f64,
}

fn ms(t: std::time::Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// The last valid index loaded per index path in this process, with the
/// header it was loaded with.
type IndexCache = Mutex<HashMap<PathBuf, (Arc<Hnsw>, IndexMeta)>>;

fn index_cache() -> &'static IndexCache {
    static CACHE: OnceLock<IndexCache> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn cached_index(path: &Path) -> Option<(Arc<Hnsw>, IndexMeta)> {
    index_cache()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(path)
        .cloned()
}

fn remember_index(path: &Path, index: Arc<Hnsw>, meta: IndexMeta) {
    index_cache()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(path.to_path_buf(), (index, meta));
}

/// Drops every cached index (tests).
pub fn clear_cache() {
    index_cache()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clear();
}

/// Default `ef_search` for queries (`search.ann.ef_search`): recall@10 is
/// about 0.96 at this value on a 20k-vector MiniLM corpus, for about 1 ms.
pub const DEFAULT_EF_SEARCH: usize = 400;

/// Top-`k` `(subject_type, subject_id, similarity)` for `query` in
/// `build_id`: the persisted index when it matches the build and model,
/// else exact search over SQLite.
pub fn search(
    store: &ProjectStore,
    project_dir: &Path,
    build_id: &str,
    fingerprint: &str,
    query: &[f32],
    k: usize,
) -> Result<(Engine, Vec<AnnHit>), String> {
    search_timed(
        store,
        project_dir,
        build_id,
        fingerprint,
        query,
        k,
        DEFAULT_EF_SEARCH,
    )
    .map(|(e, h, _)| (e, h))
}

/// [`search`] with an explicit `ef_search`, plus where the time went.
pub fn search_timed(
    store: &ProjectStore,
    project_dir: &Path,
    build_id: &str,
    fingerprint: &str,
    query: &[f32],
    k: usize,
    ef_search: usize,
) -> Result<(Engine, Vec<AnnHit>, AnnTimings), String> {
    let split = |hits: Vec<(String, f32)>| {
        hits.into_iter()
            .filter_map(|(l, s)| split_label(&l).map(|(t, id)| (t.to_owned(), id.to_owned(), s)))
            .collect::<Vec<_>>()
    };
    let mut timings = AnnTimings::default();
    let t = std::time::Instant::now();
    let current = store
        .embeddings_revision(build_id)
        .map_err(|e| e.to_string())?;
    timings.freshness_ms = ms(t);
    let usable = |m: &IndexMeta| {
        m.build_id == build_id
            && m.fingerprint == fingerprint
            && m.dims == query.len()
            && m.content == current
    };
    let path = index_path(project_dir);
    let t = std::time::Instant::now();
    let index = match cached_index(&path).filter(|c| usable(&c.1)) {
        Some(hit) => {
            crate::counters::ann_cache_hit();
            Some(hit.0)
        }
        None => load(&path).filter(|(_, m)| usable(m)).map(|(index, meta)| {
            let index = Arc::new(index);
            remember_index(&path, index.clone(), meta);
            index
        }),
    };
    timings.load_ms = ms(t);
    if let Some(index) = index {
        let t = std::time::Instant::now();
        let hits = split(index.search_ef(query, k, ef_search));
        timings.search_ms = ms(t);
        return Ok((Engine::Hnsw, hits, timings));
    }
    let t = std::time::Instant::now();
    let all = store
        .embeddings(build_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|e| e.model_fingerprint == fingerprint && e.vector.len() == query.len())
        .map(|e| (label(&e.subject_type, &e.subject_id), e.vector))
        .collect::<Vec<_>>();
    let hits = exact_top_k(
        query,
        all.iter().map(|(l, v)| (l.as_str(), v.as_slice())),
        k,
    );
    timings.search_ms = ms(t);
    Ok((Engine::Exact, split(hits), timings))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (Hnsw, IndexMeta) {
        let mut h = Hnsw::new(4, HnswParams::default());
        for i in 0..50u32 {
            let a = i as f32 * 0.37;
            let v = [a.cos(), a.sin(), 0.5, -0.5];
            let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            h.insert(&format!("chunk:{i}"), &v.map(|x| x / n));
        }
        h.remove("chunk:3");
        let meta = IndexMeta {
            format_version: FORMAT_VERSION,
            fingerprint: "fp".into(),
            build_id: "b1".into(),
            dims: 4,
            content: "digest".into(),
        };
        (h, meta)
    }

    #[test]
    fn round_trips_exactly() {
        let (h, meta) = sample();
        let bytes = encode(&h, &meta);
        let (back, m) = decode(&bytes).unwrap();
        assert_eq!(m, meta);
        assert_eq!(encode(&back, &m), bytes);
        assert_eq!(back.len(), 49);
        assert!(!back.contains("chunk:3"));
        let q = [1.0, 0.0, 0.0, 0.0];
        assert_eq!(back.search(&q, 5), h.search(&q, 5));
    }

    #[test]
    fn rejects_corruption_truncation_and_other_versions() {
        let (h, meta) = sample();
        let bytes = encode(&h, &meta);
        for cut in [0, 7, 40, bytes.len() / 2, bytes.len() - 1] {
            assert!(decode(&bytes[..cut]).is_none(), "truncated at {cut}");
        }
        let mut flipped = bytes.clone();
        flipped[100] ^= 0x40;
        assert!(decode(&flipped).is_none());
        let mut other = meta.clone();
        other.format_version = FORMAT_VERSION + 1;
        assert!(decode(&encode(&h, &other)).is_none());
    }

    #[test]
    fn delta_log_replays_to_the_same_graph_and_stops_at_corruption() {
        let tmp = tempfile::tempdir().unwrap();
        let path = index_path(tmp.path());
        let (mut h, meta) = sample();
        save(&path, &h, &meta).unwrap();
        let digest = load_full(&path).unwrap().base_digest;
        let mut ops1 = vec![DeltaOp::Remove("chunk:7".into())];
        for i in 50..55u32 {
            let a = i as f32 * 0.37;
            let v = vec![a.sin(), a.cos(), -0.5, 0.5];
            ops1.push(DeltaOp::Insert(format!("chunk:{i}"), v));
        }
        let mut m1 = meta.clone();
        m1.content = "c1".into();
        let first = append_delta(&path, &digest, &m1, &ops1).unwrap();
        let ops2 = vec![DeltaOp::Remove("chunk:9".into())];
        let mut m2 = meta.clone();
        m2.content = "c2".into();
        append_delta(&path, &digest, &m2, &ops2).unwrap();
        apply(&mut h, &ops1);
        let after1 = encode(&h, &m1);
        apply(&mut h, &ops2);

        let l = load_full(&path).unwrap();
        assert!(l.delta_clean);
        assert_eq!(l.meta, m2);
        assert_eq!(encode(&l.index, &l.meta), encode(&h, &m2), "exact replay");

        // A torn second record: replay keeps the first one only.
        let delta = delta_path(&path);
        let mut log = std::fs::read(&delta).unwrap();
        log.truncate(log.len() - 3);
        std::fs::write(&delta, &log).unwrap();
        let l = load_full(&path).unwrap();
        assert!(!l.delta_clean);
        assert_eq!(l.meta.content, "c1");
        assert_eq!(encode(&l.index, &l.meta), after1);
        // A flipped byte in the first record stops replay before it.
        log[40 + 6] ^= 0x10;
        std::fs::write(&delta, &log).unwrap();
        let l = load_full(&path).unwrap();
        assert_eq!((l.delta_clean, l.meta.content.as_str()), (false, "digest"));
        assert!(first > 40);

        // A log of another base is ignored, and a full save removes it.
        let (other, om) = sample();
        save(&path, &other, &om).unwrap();
        assert!(!delta.exists());
        std::fs::write(&delta, &log).unwrap();
        let l = load_full(&path).unwrap();
        assert_eq!((l.delta_clean, l.meta.content.as_str()), (false, "digest"));
    }

    #[test]
    fn save_is_atomic_and_load_tolerates_missing_files() {
        let tmp = tempfile::tempdir().unwrap();
        let path = index_path(tmp.path());
        assert!(load(&path).is_none());
        let (h, meta) = sample();
        save(&path, &h, &meta).unwrap();
        assert!(load(&path).is_some());
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("index.hnsw")]);
    }
}
