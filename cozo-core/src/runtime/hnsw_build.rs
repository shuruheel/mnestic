/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 * Portions Copyright 2023, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Flat in-RAM HNSW bulk builder (mnestic fork).
//!
//! `::hnsw create` used to construct the graph through the temp-store BTreeMap,
//! paying tuple encode/decode, `CompoundKey` hashing and allocator traffic on
//! every neighbour access — measured at >50% of build CPU. This module builds
//! the graph in flat, integer-indexed memory instead (vector slab + per-node
//! adjacency, the hnswlib/pgvector/Lucene layout) with optional parallel
//! insertion guarded by per-node locks, then serialises the finished graph into
//! the index-relation tuple format in one pass. The on-disk format, the search
//! path, and incremental maintenance (`hnsw_put`/`hnsw_remove`) are unchanged.
//!
//! Insertion mirrors `hnsw.rs::hnsw_put_vector` (greedy descent, ef_construction
//! search per level, the select-neighbours heuristic with `extend_candidates` /
//! `keep_pruned_connections`, backlink shrinking), with two deliberate
//! deviations, both safe only because this path never runs concurrently with
//! the steady-state writer:
//! - shrinking an overflowing neighbour never extends candidates (avoids
//!   taking two node locks at once, which could deadlock under parallelism);
//! - a node never links to itself even if a concurrent insertion has already
//!   back-linked it into a neighbour list it then searches.
//!
//! Thread count: `MNESTIC_INDEX_BUILD_THREADS` (0 or unset = all available
//! cores; 1 = serial insertion in scan order, matching the old build).

use std::cmp::{max, Reverse};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, RwLock};

use miette::{bail, Result};
use ordered_float::OrderedFloat;
use priority_queue::PriorityQueue;
use rustc_hash::FxHashSet;

use crate::data::relation::VecElementType;
use crate::data::value::Vector;
use crate::parse::sys::HnswDistance;
use crate::runtime::hnsw::HnswIndexManifest;

/// One indexed vector: its owning row's key part plus the field/sub-field that
/// held it, and the SHA-256 the self-row stores for change detection.
pub(crate) struct NodeMeta {
    pub(crate) key: Vec<crate::DataValue>,
    pub(crate) field_idx: usize,
    pub(crate) sub_idx: i32,
    pub(crate) hash: Vec<u8>,
}

enum VecSlab {
    F32(Vec<f32>),
    F64(Vec<f64>),
}

/// Eight-accumulator unrolled dot/L2, the same shape as ndarray's
/// `unrolled_dot` the query path uses, so build-time distances agree with
/// query-time distances to float-rounding noise.
macro_rules! unrolled {
    ($a:expr, $b:expr, $t:ty, $f:expr) => {{
        let a = $a;
        let b = $b;
        let mut acc: [$t; 8] = [0.0; 8];
        let chunks = a.len() / 8;
        for c in 0..chunks {
            let i = c * 8;
            for j in 0..8 {
                acc[j] += $f(a[i + j], b[i + j]);
            }
        }
        let mut sum: $t = 0.0;
        for v in &mut acc {
            sum += *v;
        }
        for i in chunks * 8..a.len() {
            sum += $f(a[i], b[i]);
        }
        sum
    }};
}

impl VecSlab {
    fn dot(&self, i: usize, j: usize, dim: usize) -> f64 {
        match self {
            VecSlab::F32(s) => unrolled!(
                &s[i * dim..(i + 1) * dim],
                &s[j * dim..(j + 1) * dim],
                f32,
                |x: f32, y: f32| x * y
            ) as f64,
            VecSlab::F64(s) => unrolled!(
                &s[i * dim..(i + 1) * dim],
                &s[j * dim..(j + 1) * dim],
                f64,
                |x: f64, y: f64| x * y
            ),
        }
    }
    fn l2(&self, i: usize, j: usize, dim: usize) -> f64 {
        match self {
            VecSlab::F32(s) => unrolled!(
                &s[i * dim..(i + 1) * dim],
                &s[j * dim..(j + 1) * dim],
                f32,
                |x: f32, y: f32| (x - y) * (x - y)
            ) as f64,
            VecSlab::F64(s) => unrolled!(
                &s[i * dim..(i + 1) * dim],
                &s[j * dim..(j + 1) * dim],
                f64,
                |x: f64, y: f64| (x - y) * (x - y)
            ),
        }
    }
    fn self_dot(&self, i: usize, dim: usize) -> f64 {
        self.dot(i, i, dim)
    }
}

/// Per-worker scratch reused across insertions.
#[derive(Default)]
struct SearchBuffers {
    /// max-queue of the current nearest set, carried across levels of one insert
    found: PriorityQueue<u32, OrderedFloat<f64>>,
    visited: FxHashSet<u32>,
    /// min-queue (via `Reverse`) of nodes still to expand
    candidates: PriorityQueue<u32, Reverse<OrderedFloat<f64>>>,
    neigh: Vec<u32>,
    sel_candidates: PriorityQueue<u32, Reverse<OrderedFloat<f64>>>,
    sel_discarded: PriorityQueue<u32, Reverse<OrderedFloat<f64>>>,
}

pub(crate) struct FlatHnswGraph {
    pub(crate) metas: Vec<NodeMeta>,
    /// towers[i][d] = outgoing neighbours of node i at level `-(d as i64)`
    pub(crate) towers: Vec<Vec<Vec<(u32, f64)>>>,
    /// (node, topmost level) — the entry point a search will discover
    pub(crate) entry: Option<(u32, i64)>,
}

pub(crate) struct FlatHnswBuilder {
    dim: usize,
    distance: HnswDistance,
    m_max: usize,
    m_max0: usize,
    ef_construction: usize,
    extend_candidates: bool,
    keep_pruned_connections: bool,
    slab: VecSlab,
    norms_sq: Vec<f64>,
    levels: Vec<i64>,
    towers: Vec<Mutex<Vec<Vec<(u32, f64)>>>>,
    entry: RwLock<Option<(u32, i64)>>,
    metas: Vec<NodeMeta>,
}

#[inline]
fn depth_of(level: i64) -> usize {
    (-level) as usize
}

impl FlatHnswBuilder {
    pub(crate) fn new(manifest: &HnswIndexManifest) -> Self {
        Self {
            dim: manifest.vec_dim,
            distance: manifest.distance,
            m_max: manifest.m_max,
            m_max0: manifest.m_max0,
            ef_construction: manifest.ef_construction,
            extend_candidates: manifest.extend_candidates,
            keep_pruned_connections: manifest.keep_pruned_connections,
            slab: match manifest.dtype {
                VecElementType::F32 => VecSlab::F32(vec![]),
                VecElementType::F64 => VecSlab::F64(vec![]),
            },
            norms_sq: vec![],
            levels: vec![],
            towers: vec![],
            entry: RwLock::new(None),
            metas: vec![],
        }
    }

    /// Add one vector; nodes are inserted in the order added.
    pub(crate) fn add_node(&mut self, meta: NodeMeta, vec: &Vector) -> Result<()> {
        if vec.len() != self.dim {
            bail!(
                "vector dimension mismatch during HNSW build: expected {}, got {}",
                self.dim,
                vec.len()
            );
        }
        match (&mut self.slab, vec) {
            (VecSlab::F32(s), Vector::F32(v)) => s.extend(v.iter()),
            (VecSlab::F64(s), Vector::F64(v)) => s.extend(v.iter()),
            _ => bail!("vector element type does not match the index dtype"),
        }
        self.metas.push(meta);
        Ok(())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.metas.is_empty()
    }

    #[inline]
    fn dist(&self, i: u32, j: u32) -> f64 {
        let (i, j) = (i as usize, j as usize);
        match self.distance {
            HnswDistance::L2 => self.slab.l2(i, j, self.dim),
            HnswDistance::Cosine => {
                1.0 - self.slab.dot(i, j, self.dim) / (self.norms_sq[i] * self.norms_sq[j]).sqrt()
            }
            HnswDistance::InnerProduct => 1.0 - self.slab.dot(i, j, self.dim),
        }
    }

    /// Build the graph. `manifest.get_random_level()` semantics are preserved
    /// by drawing each node's level up front from the same distribution.
    pub(crate) fn build(mut self, manifest: &HnswIndexManifest, threads: usize) -> FlatHnswGraph {
        let n = self.metas.len();
        if matches!(self.distance, HnswDistance::Cosine) {
            self.norms_sq = (0..n).map(|i| self.slab.self_dot(i, self.dim)).collect();
        }
        self.levels = (0..n).map(|_| manifest.get_random_level()).collect();
        self.towers = self
            .levels
            .iter()
            .map(|l| Mutex::new(vec![vec![]; depth_of(*l) + 1]))
            .collect();

        // First node seeds the graph and the entry point, as in the serial path.
        *self.entry.write().unwrap() = Some((0, self.levels[0]));

        if n > 1 {
            let threads = threads.max(1).min(n - 1);
            let counter = AtomicUsize::new(1);
            if threads == 1 {
                let mut bufs = SearchBuffers::default();
                for i in 1..n {
                    self.insert(i as u32, &mut bufs);
                }
            } else {
                std::thread::scope(|s| {
                    for _ in 0..threads {
                        s.spawn(|| {
                            let mut bufs = SearchBuffers::default();
                            loop {
                                let i = counter.fetch_add(1, Ordering::Relaxed);
                                if i >= n {
                                    break;
                                }
                                self.insert(i as u32, &mut bufs);
                            }
                        });
                    }
                });
            }
        }

        let entry = *self.entry.read().unwrap();
        FlatHnswGraph {
            metas: self.metas,
            towers: self
                .towers
                .into_iter()
                .map(|m| m.into_inner().unwrap())
                .collect(),
            entry,
        }
    }

    /// Mirrors `hnsw_put_vector` for the fresh-build case.
    fn insert(&self, q: u32, bufs: &mut SearchBuffers) {
        let target_level = self.levels[q as usize];
        let (ep_id, ep_top) = self.entry.read().unwrap().expect("entry point must exist");

        bufs.found.clear();
        bufs.found.push(ep_id, OrderedFloat(self.dist(q, ep_id)));

        // Greedy descent through levels above the node's own top level.
        for level in ep_top..target_level {
            self.search_level(q, 1, level, bufs);
        }
        // Link levels, from the node's top (or the graph's top, whichever is
        // lower in the hierarchy) down to the base layer.
        for level in max(target_level, ep_top)..=0 {
            let m_max = if level == 0 { self.m_max0 } else { self.m_max };
            self.search_level(q, self.ef_construction, level, bufs);
            let neighbours = self.select_heuristic(
                q,
                m_max,
                level,
                self.extend_candidates,
                &bufs.found,
                &mut bufs.sel_candidates,
                &mut bufs.sel_discarded,
                &mut bufs.neigh,
            );
            {
                let mut tower = self.towers[q as usize].lock().unwrap();
                let list = &mut tower[depth_of(level)];
                if list.is_empty() {
                    *list = neighbours.clone();
                } else {
                    // A concurrent insertion has already back-linked into us;
                    // replacing the list would sever that edge (and, on
                    // chain-shaped data, break graph connectivity). Merge.
                    for &(nb, dist) in &neighbours {
                        if !list.iter().any(|&(e, _)| e == nb) {
                            list.push((nb, dist));
                        }
                    }
                    if list.len() > m_max {
                        *list = self.select_from_list(list, m_max);
                    }
                }
            }
            for &(nb, dist) in &neighbours {
                let mut tower = self.towers[nb as usize].lock().unwrap();
                let list = &mut tower[depth_of(level)];
                // The neighbour may already hold this edge (it selected us
                // concurrently); a duplicate would skew its stored degree.
                if !list.iter().any(|&(e, _)| e == q) {
                    list.push((q, dist));
                }
                if list.len() > m_max {
                    // Re-select the neighbour's links under its own lock only:
                    // candidates are its current list, distances come from the
                    // slab, and (unlike the steady-state path) we never extend,
                    // so no second lock is taken.
                    let shrunk = self.select_from_list(list, m_max);
                    *list = shrunk;
                }
            }
        }

        if target_level < ep_top {
            let mut entry = self.entry.write().unwrap();
            match *entry {
                Some((_, top)) if target_level < top => *entry = Some((q, target_level)),
                _ => {}
            }
        }
    }

    /// Mirrors `hnsw_search_level`: expands `bufs.found` (a max-queue capped at
    /// `ef`) with the closest reachable nodes at `level`.
    fn search_level(&self, q: u32, ef: usize, level: i64, bufs: &mut SearchBuffers) {
        let d = depth_of(level);
        bufs.visited.clear();
        bufs.candidates.clear();
        bufs.visited.insert(q);
        for (item, prio) in bufs.found.iter() {
            bufs.visited.insert(*item);
            bufs.candidates.push(*item, Reverse(*prio));
        }
        while let Some((cand, Reverse(OrderedFloat(cand_dist)))) = bufs.candidates.pop() {
            let (_, OrderedFloat(furthest)) = bufs.found.peek().expect("found is never empty");
            if cand_dist > *furthest {
                break;
            }
            bufs.neigh.clear();
            {
                let tower = self.towers[cand as usize].lock().unwrap();
                if let Some(list) = tower.get(d) {
                    bufs.neigh.extend(list.iter().map(|&(nb, _)| nb));
                }
            }
            for k in 0..bufs.neigh.len() {
                let nb = bufs.neigh[k];
                if !bufs.visited.insert(nb) {
                    continue;
                }
                let nb_dist = self.dist(q, nb);
                let (_, OrderedFloat(furthest)) = bufs.found.peek().expect("found is never empty");
                if bufs.found.len() < ef || nb_dist < *furthest {
                    bufs.candidates.push(nb, Reverse(OrderedFloat(nb_dist)));
                    bufs.found.push(nb, OrderedFloat(nb_dist));
                    if bufs.found.len() > ef {
                        bufs.found.pop();
                    }
                }
            }
        }
    }

    /// Mirrors `hnsw_select_neighbours_heuristic`: greedily keep the nearest
    /// candidate unless it is closer to an already-kept node than to the query
    /// (diversity pruning), optionally back-filling from the pruned set.
    /// `q` is the node being inserted, excluded from candidacy; `extend`
    /// carries the manifest's `extend_candidates` (always false for shrinks,
    /// see `insert`).
    #[allow(clippy::too_many_arguments)]
    fn select_heuristic(
        &self,
        q: u32,
        m: usize,
        level: i64,
        extend: bool,
        found: &PriorityQueue<u32, OrderedFloat<f64>>,
        candidates: &mut PriorityQueue<u32, Reverse<OrderedFloat<f64>>>,
        discarded: &mut PriorityQueue<u32, Reverse<OrderedFloat<f64>>>,
        neigh: &mut Vec<u32>,
    ) -> Vec<(u32, f64)> {
        candidates.clear();
        for (item, dist) in found.iter() {
            candidates.push(*item, Reverse(*dist));
        }
        if extend {
            let d = depth_of(level);
            for (item, _) in found.iter() {
                neigh.clear();
                {
                    let tower = self.towers[*item as usize].lock().unwrap();
                    if let Some(list) = tower.get(d) {
                        neigh.extend(list.iter().map(|&(nb, _)| nb));
                    }
                }
                for &nb in neigh.iter() {
                    if nb == q {
                        continue;
                    }
                    candidates.push(nb, Reverse(OrderedFloat(self.dist(q, nb))));
                }
            }
        }
        self.prune_candidates(candidates, discarded, m)
    }

    /// Shrink helper: re-select a node's neighbour list from its current list.
    /// Equivalent to the steady-state shrink with extension disabled; distances
    /// to the node are already stored on the list entries.
    fn select_from_list(&self, list: &[(u32, f64)], m: usize) -> Vec<(u32, f64)> {
        let mut candidates: PriorityQueue<u32, Reverse<OrderedFloat<f64>>> =
            PriorityQueue::with_capacity(list.len());
        for &(nb, dist) in list {
            candidates.push(nb, Reverse(OrderedFloat(dist)));
        }
        let mut discarded: PriorityQueue<u32, Reverse<OrderedFloat<f64>>> = PriorityQueue::new();
        self.prune_candidates(&mut candidates, &mut discarded, m)
    }

    /// The shared diversity-pruning core of the two selectors above.
    fn prune_candidates(
        &self,
        candidates: &mut PriorityQueue<u32, Reverse<OrderedFloat<f64>>>,
        discarded: &mut PriorityQueue<u32, Reverse<OrderedFloat<f64>>>,
        m: usize,
    ) -> Vec<(u32, f64)> {
        discarded.clear();
        let mut ret: Vec<(u32, f64)> = Vec::with_capacity(m);
        while ret.len() < m {
            let Some((cand, Reverse(OrderedFloat(cand_dist)))) = candidates.pop() else {
                break;
            };
            let mut should_add = true;
            for &(existing, _) in ret.iter() {
                if self.dist(existing, cand) < cand_dist {
                    should_add = false;
                    break;
                }
            }
            if should_add {
                ret.push((cand, cand_dist));
            } else if self.keep_pruned_connections {
                discarded.push(cand, Reverse(OrderedFloat(cand_dist)));
            }
        }
        if self.keep_pruned_connections {
            while ret.len() < m {
                let Some((cand, Reverse(OrderedFloat(cand_dist)))) = discarded.pop() else {
                    break;
                };
                ret.push((cand, cand_dist));
            }
        }
        ret
    }
}

/// Resolve the index-build thread count (shared by the HNSW and FTS bulk
/// builds): `MNESTIC_INDEX_BUILD_THREADS` wins (1 = serial, matching the old
/// builds' insertion order); otherwise all available cores.
pub(crate) fn build_threads() -> usize {
    match std::env::var("MNESTIC_INDEX_BUILD_THREADS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
    {
        Some(0) | None => std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
        Some(n) => n,
    }
}
