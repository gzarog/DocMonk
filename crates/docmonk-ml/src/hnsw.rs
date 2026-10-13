//! A small, deterministic HNSW index (Malkov & Yashunin) over L2-normalized
//! vectors, scored by inner product (= cosine similarity).
//!
//! Deletions are tombstones: deleted nodes still route searches but are
//! never returned. Callers rebuild once tombstones dominate. Level draws use
//! a seeded generator, so the same insertion sequence always yields the
//! same graph.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

/// Multiplicative hasher for `u32` node ids (visited sets are hot; SipHash
/// is needlessly slow for them and there is no DoS surface).
#[derive(Default)]
struct NodeHasher(u64);

impl Hasher for NodeHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = (self.0 ^ u64::from(*b)).wrapping_mul(0x100_0000_01b3);
        }
    }
    fn write_u32(&mut self, n: u32) {
        self.0 = (u64::from(n) ^ 0x9e37_79b9_7f4a_7c15).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    }
}

type NodeSet = HashSet<u32, BuildHasherDefault<NodeHasher>>;

/// Construction/search parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HnswParams {
    /// Neighbours per node above layer 0 (layer 0 keeps `2 * m`).
    pub m: usize,
    pub ef_construction: usize,
    pub ef_search: usize,
    pub seed: u64,
}

impl Default for HnswParams {
    fn default() -> Self {
        Self {
            m: 16,
            ef_construction: 128,
            ef_search: 96,
            seed: 0x5241_474d_4f4e_4b21,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Scored {
    sim: f32,
    node: u32,
}

impl Eq for Scored {}

impl Ord for Scored {
    fn cmp(&self, other: &Self) -> Ordering {
        self.sim
            .total_cmp(&other.sim)
            .then_with(|| other.node.cmp(&self.node))
    }
}

impl PartialOrd for Scored {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Min-heap adapter (worst candidate on top).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Worst(std::cmp::Reverse<Scored>);

#[derive(Debug, Clone)]
pub struct Hnsw {
    pub(crate) params: HnswParams,
    pub(crate) dims: usize,
    pub(crate) vectors: Vec<f32>,
    pub(crate) labels: Vec<String>,
    /// `links[node][layer]`.
    pub(crate) links: Vec<Vec<Vec<u32>>>,
    pub(crate) deleted: Vec<bool>,
    pub(crate) entry: Option<u32>,
    pub(crate) rng: u64,
    by_label: HashMap<String, u32>,
    live: usize,
}

/// Inner product with 8 independent accumulators so the compiler can
/// vectorize it (a single running sum is a serial dependency chain).
#[inline]
pub(crate) fn dot(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let (a, b) = (&a[..n], &b[..n]);
    let mut acc = [0f32; 8];
    let chunks = n / 8;
    for i in 0..chunks {
        let (x, y) = (&a[i * 8..i * 8 + 8], &b[i * 8..i * 8 + 8]);
        for l in 0..8 {
            acc[l] += x[l] * y[l];
        }
    }
    let mut tail = 0f32;
    for i in chunks * 8..n {
        tail += a[i] * b[i];
    }
    acc.iter().sum::<f32>() + tail
}

impl Hnsw {
    pub fn new(dims: usize, params: HnswParams) -> Self {
        Self {
            params,
            dims,
            vectors: Vec::new(),
            labels: Vec::new(),
            links: Vec::new(),
            deleted: Vec::new(),
            entry: None,
            rng: params.seed,
            by_label: HashMap::new(),
            live: 0,
        }
    }

    /// Reassembles an index from its persisted parts (see `ann::load`).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_parts(
        params: HnswParams,
        dims: usize,
        vectors: Vec<f32>,
        labels: Vec<String>,
        links: Vec<Vec<Vec<u32>>>,
        deleted: Vec<bool>,
        entry: Option<u32>,
        rng: u64,
    ) -> Self {
        let by_label = labels
            .iter()
            .enumerate()
            .filter(|(i, _)| !deleted[*i])
            .map(|(i, l)| (l.clone(), i as u32))
            .collect();
        let live = deleted.iter().filter(|d| !**d).count();
        Self {
            params,
            dims,
            vectors,
            labels,
            links,
            deleted,
            entry,
            rng,
            by_label,
            live,
        }
    }

    pub fn dims(&self) -> usize {
        self.dims
    }

    /// Live (non-deleted) entries.
    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// Nodes including tombstones.
    pub fn capacity(&self) -> usize {
        self.labels.len()
    }

    pub fn contains(&self, label: &str) -> bool {
        self.by_label.contains_key(label)
    }

    pub fn labels(&self) -> impl Iterator<Item = &str> {
        self.by_label.keys().map(String::as_str)
    }

    fn vector(&self, node: u32) -> &[f32] {
        let i = node as usize * self.dims;
        &self.vectors[i..i + self.dims]
    }

    fn next_level(&mut self) -> usize {
        // splitmix64
        self.rng = self.rng.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        let u = ((z >> 11) as f64 + 0.5) / (1u64 << 53) as f64;
        let ml = 1.0 / (self.params.m.max(2) as f64).ln();
        ((-u.ln() * ml).floor() as usize).min(16)
    }

    fn max_links(&self, layer: usize) -> usize {
        if layer == 0 {
            self.params.m * 2
        } else {
            self.params.m
        }
    }

    fn search_layer(&self, q: &[f32], entries: &[u32], ef: usize, layer: usize) -> Vec<Scored> {
        let mut visited: NodeSet = entries.iter().copied().collect();
        let mut candidates: BinaryHeap<Scored> = BinaryHeap::new();
        let mut best: BinaryHeap<Worst> = BinaryHeap::new();
        for &e in entries {
            let s = Scored {
                sim: dot(q, self.vector(e)),
                node: e,
            };
            candidates.push(s);
            best.push(Worst(std::cmp::Reverse(s)));
        }
        while let Some(c) = candidates.pop() {
            let worst = best.peek().map_or(f32::NEG_INFINITY, |w| w.0 .0.sim);
            if c.sim < worst && best.len() >= ef {
                break;
            }
            let Some(neigh) = self.links[c.node as usize].get(layer) else {
                continue;
            };
            for &n in neigh {
                if !visited.insert(n) {
                    continue;
                }
                let s = Scored {
                    sim: dot(q, self.vector(n)),
                    node: n,
                };
                let worst = best.peek().map_or(f32::NEG_INFINITY, |w| w.0 .0.sim);
                if best.len() < ef || s.sim > worst {
                    candidates.push(s);
                    best.push(Worst(std::cmp::Reverse(s)));
                    if best.len() > ef {
                        best.pop();
                    }
                }
            }
        }
        let mut out: Vec<Scored> = best.into_iter().map(|w| w.0 .0).collect();
        out.sort_by(|a, b| b.cmp(a));
        out
    }

    /// The HNSW neighbour-selection heuristic: keep a candidate only if it is
    /// closer to the base than to every neighbour already kept (diversity),
    /// then top up with the nearest rejected ones.
    fn select(&self, candidates: &[Scored], m: usize) -> Vec<u32> {
        let mut kept: Vec<Scored> = Vec::with_capacity(m);
        let mut rejected = Vec::new();
        for &c in candidates {
            if kept.len() >= m {
                break;
            }
            let cv = self.vector(c.node);
            if kept.iter().all(|k| dot(cv, self.vector(k.node)) < c.sim) {
                kept.push(c);
            } else {
                rejected.push(c);
            }
        }
        for r in rejected {
            if kept.len() >= m {
                break;
            }
            kept.push(r);
        }
        kept.into_iter().map(|s| s.node).collect()
    }

    /// Inserts (or replaces) `label`. `vector` must have `dims` entries.
    pub fn insert(&mut self, label: &str, vector: &[f32]) {
        assert_eq!(vector.len(), self.dims, "vector dimensionality");
        self.remove(label);
        let node = self.labels.len() as u32;
        let level = self.next_level();
        self.vectors.extend_from_slice(vector);
        self.labels.push(label.to_owned());
        self.links.push(vec![Vec::new(); level + 1]);
        self.deleted.push(false);
        self.by_label.insert(label.to_owned(), node);
        self.live += 1;
        let Some(entry) = self.entry else {
            self.entry = Some(node);
            return;
        };
        let top = self.links[entry as usize].len() - 1;
        let mut cur = vec![entry];
        for layer in (level + 1..=top).rev() {
            cur = vec![self.search_layer(vector, &cur, 1, layer)[0].node];
        }
        for layer in (0..=level.min(top)).rev() {
            let found = self.search_layer(vector, &cur, self.params.ef_construction, layer);
            let chosen = self.select(&found, self.params.m);
            self.links[node as usize][layer] = chosen.clone();
            for &n in &chosen {
                let max = self.max_links(layer);
                let list = &mut self.links[n as usize][layer];
                list.push(node);
                if list.len() > max {
                    let base = self.vector(n).to_vec();
                    let mut scored: Vec<Scored> = self.links[n as usize][layer]
                        .iter()
                        .map(|&x| Scored {
                            sim: dot(&base, self.vector(x)),
                            node: x,
                        })
                        .collect();
                    scored.sort_by(|a, b| b.cmp(a));
                    let pruned = self.select(&scored, max);
                    self.links[n as usize][layer] = pruned;
                }
            }
            cur = found.iter().map(|s| s.node).collect();
        }
        if level > top {
            self.entry = Some(node);
        }
    }

    /// Tombstones `label`; returns whether it was present.
    pub fn remove(&mut self, label: &str) -> bool {
        match self.by_label.remove(label) {
            Some(node) => {
                self.deleted[node as usize] = true;
                self.live -= 1;
                true
            }
            None => false,
        }
    }

    /// Share of tombstoned nodes.
    pub fn tombstone_ratio(&self) -> f64 {
        if self.labels.is_empty() {
            0.0
        } else {
            1.0 - self.live as f64 / self.labels.len() as f64
        }
    }

    /// Top-`k` live labels by cosine similarity (best first; ties by label).
    pub fn search(&self, q: &[f32], k: usize) -> Vec<(String, f32)> {
        let Some(entry) = self.entry else {
            return Vec::new();
        };
        if k == 0 || self.live == 0 {
            return Vec::new();
        }
        let top = self.links[entry as usize].len() - 1;
        let mut cur = vec![entry];
        for layer in (1..=top).rev() {
            cur = vec![self.search_layer(q, &cur, 1, layer)[0].node];
        }
        // Widen the beam by the tombstone count so deleted nodes cannot
        // crowd out live results.
        let dead = self.labels.len() - self.live;
        let ef = self.params.ef_search.max(k) + dead.min(4 * self.params.ef_search);
        let mut hits: Vec<(String, f32)> = self
            .search_layer(q, &cur, ef, 0)
            .into_iter()
            .filter(|s| !self.deleted[s.node as usize])
            .map(|s| (self.labels[s.node as usize].clone(), s.sim))
            .collect();
        hits.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        hits.truncate(k);
        hits
    }

    /// A compacted copy without tombstones (rebuilt by re-insertion in label
    /// order, so the result is deterministic).
    pub fn compacted(&self) -> Self {
        let mut live: Vec<(&str, u32)> = self
            .by_label
            .iter()
            .map(|(l, n)| (l.as_str(), *n))
            .collect();
        live.sort();
        let mut out = Self::new(self.dims, self.params);
        for (label, node) in live {
            out.insert(label, self.vector(node));
        }
        out
    }
}

/// Exact top-`k` over `(label, vector)` pairs (the always-correct fallback
/// and the recall reference).
pub fn exact_top_k<'a>(
    q: &[f32],
    items: impl IntoIterator<Item = (&'a str, &'a [f32])>,
    k: usize,
) -> Vec<(String, f32)> {
    let mut all: Vec<(String, f32)> = items
        .into_iter()
        .map(|(l, v)| (l.to_owned(), dot(q, v)))
        .collect();
    all.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    all.truncate(k);
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(seed: u64, dims: usize) -> Vec<f32> {
        let mut s = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        let mut v: Vec<f32> = (0..dims)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s % 2001) as f32 / 1000.0 - 1.0
            })
            .collect();
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter_mut().for_each(|x| *x /= n);
        v
    }

    fn build(n: usize, dims: usize) -> (Hnsw, Vec<(String, Vec<f32>)>) {
        let mut h = Hnsw::new(dims, HnswParams::default());
        let items: Vec<(String, Vec<f32>)> = (0..n)
            .map(|i| (format!("e{i}"), unit(i as u64, dims)))
            .collect();
        for (l, v) in &items {
            h.insert(l, v);
        }
        (h, items)
    }

    fn recall(h: &Hnsw, items: &[(String, Vec<f32>)], k: usize, queries: u64) -> f64 {
        let mut hit = 0;
        for q in 0..queries {
            let qv = unit(10_000 + q, h.dims());
            let truth = exact_top_k(
                &qv,
                items
                    .iter()
                    .filter(|(l, _)| h.contains(l))
                    .map(|(l, v)| (l.as_str(), v.as_slice())),
                k,
            );
            let got: HashSet<String> = h.search(&qv, k).into_iter().map(|x| x.0).collect();
            hit += truth.iter().filter(|t| got.contains(&t.0)).count();
        }
        hit as f64 / (k as u64 * queries) as f64
    }

    #[test]
    fn recall_is_high_against_exact_search() {
        let (h, items) = build(3000, 32);
        assert_eq!(h.len(), 3000);
        let r = recall(&h, &items, 10, 50);
        assert!(r >= 0.95, "recall@10 = {r}");
    }

    #[test]
    fn deletes_are_never_returned_and_compaction_keeps_recall() {
        let (mut h, items) = build(2000, 24);
        for i in (0..2000).step_by(3) {
            assert!(h.remove(&format!("e{i}")));
        }
        assert!(!h.remove("e0"));
        for q in 0..20 {
            for (l, _) in h.search(&unit(500 + q, 24), 20) {
                let i: usize = l[1..].parse().unwrap();
                assert_ne!(i % 3, 0, "deleted {l} returned");
            }
        }
        assert!(recall(&h, &items, 10, 30) >= 0.9);
        let c = h.compacted();
        assert_eq!(c.capacity(), c.len());
        assert_eq!(c.len(), h.len());
        assert!(recall(&c, &items, 10, 30) >= 0.95);
    }

    #[test]
    fn construction_is_deterministic_and_reinsert_replaces() {
        let (a, _) = build(500, 16);
        let (mut b, _) = build(500, 16);
        assert_eq!(a.links, b.links);
        let q = unit(7, 16);
        assert_eq!(a.search(&q, 5), b.search(&q, 5));
        b.insert("e1", &q);
        assert_eq!(b.len(), 500);
        assert_eq!(b.search(&q, 1)[0].0, "e1");
    }
}
