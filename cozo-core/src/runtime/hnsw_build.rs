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
//! When shrinking prunes one direction of an edge, the incremental path keeps a
//! tombstoned row. The bulk builder reconstructs those rows from the finished
//! adjacency so removal can always find and delete both directions. Search
//! ignores the tombstones, so the live graph is unchanged.
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
use crate::runtime::hnsw::{
    cosine_distance, distance_is_closer, distance_is_farther, HnswIndexManifest,
};

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
    /// Tombstoned rows that restore the paired-row invariant after neighbour pruning.
    pub(crate) dead: Vec<Vec<Vec<(u32, f64)>>>,
    /// Nodes removed during in-RAM incremental catch-up (omitted on serialisation).
    pub(crate) deleted: Vec<bool>,
    /// (node, topmost level) — the entry point a search will discover
    pub(crate) entry: Option<(u32, i64)>,
}

pub(crate) struct FlatHnswBuilder {
    dim: usize,
    distance: HnswDistance,
    m_neighbours: usize,
    m_max: usize,
    m_max0: usize,
    ef_construction: usize,
    extend_candidates: bool,
    keep_pruned_connections: bool,
    slab: VecSlab,
    norms_sq: Vec<f64>,
    levels: Vec<i64>,
    towers: Vec<Mutex<Vec<Vec<(u32, f64)>>>>,
    rev_edges: Vec<Vec<Vec<u32>>>,
    deleted: Vec<bool>,
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
            m_neighbours: manifest.m_neighbours,
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
            rev_edges: vec![],
            deleted: vec![],
            entry: RwLock::new(None),
            metas: vec![],
        }
    }

    /// Add one vector; returns its node id. Nodes are inserted in the order added.
    pub(crate) fn add_node(&mut self, meta: NodeMeta, vec: &Vector) -> Result<u32> {
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
        let id = self.metas.len() as u32;
        self.metas.push(meta);
        Ok(id)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.metas.is_empty()
    }

    pub(crate) fn node_hash(&self, id: u32) -> &[u8] {
        &self.metas[id as usize].hash
    }

    #[inline]
    fn dist(&self, i: u32, j: u32) -> f64 {
        let (i, j) = (i as usize, j as usize);
        match self.distance {
            HnswDistance::L2 => self.slab.l2(i, j, self.dim),
            HnswDistance::Cosine => cosine_distance(
                self.norms_sq[i],
                self.norms_sq[j],
                self.slab.dot(i, j, self.dim),
            ),
            HnswDistance::InnerProduct => 1.0 - self.slab.dot(i, j, self.dim),
        }
    }

    /// Build the graph in-place so the caller may optionally run in-RAM
    /// incremental catch-up rounds (`prepare_incremental` / `remove_node` /
    /// `insert_dynamic`) before finalising via `into_graph`.
    pub(crate) fn build_graph(&mut self, manifest: &HnswIndexManifest, threads: usize) {
        let n = self.metas.len();
        self.deleted = vec![false; n];
        if n == 0 {
            *self.entry.get_mut().unwrap() = None;
            return;
        }
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
    }

    /// Populate reverse adjacency lists for O(degree) in-RAM node removals
    /// during detached catch-up before SST serialisation.
    pub(crate) fn prepare_incremental(&mut self) {
        if !self.rev_edges.is_empty() || self.towers.is_empty() {
            return;
        }
        let n = self.towers.len();
        let mut rev: Vec<Vec<Vec<u32>>> = self
            .levels
            .iter()
            .map(|l| vec![Vec::new(); depth_of(*l) + 1])
            .collect();
        for i in 0..n {
            if self.deleted[i] {
                continue;
            }
            let tower = self.towers[i].get_mut().unwrap();
            for (d, list) in tower.iter().enumerate() {
                for &(j, _) in list {
                    rev[j as usize][d].push(i as u32);
                }
            }
        }
        self.rev_edges = rev;
    }

    /// Remove a previously added node from the in-RAM graph, stripping all
    /// incoming and outgoing edges while preserving the 0.19.2 orphan guard
    /// (every surviving out-neighbour whose sole incoming edge was `id -> out`
    /// is reconnected from a live in-neighbour within `m_max`, emitting
    /// `hnsw.repair_fallback` if no reconnection fits).
    pub(crate) fn remove_node(&mut self, id: u32, relation: &str) {
        let idx = id as usize;
        if idx >= self.deleted.len() || self.deleted[idx] {
            return;
        }
        self.deleted[idx] = true;
        let my_tower = std::mem::take(self.towers[idx].get_mut().unwrap());
        let my_rev = std::mem::take(&mut self.rev_edges[idx]);
        for d in 0..my_tower.len() {
            let m_max = if d == 0 { self.m_max0 } else { self.m_max };
            let mut surviving = Vec::with_capacity(my_tower[d].len());
            for &(nb, _) in &my_tower[d] {
                if !self.deleted[nb as usize] {
                    surviving.push(nb);
                    self.rev_edges[nb as usize][d].retain(|&u| u != id);
                }
            }
            let mut live_ins = Vec::with_capacity(my_rev[d].len());
            for &in_nb in &my_rev[d] {
                let in_idx = in_nb as usize;
                if self.deleted[in_idx] {
                    continue;
                }
                self.towers[in_idx].get_mut().unwrap()[d].retain(|&(dst, _)| dst != id);
                live_ins.push(in_nb);
            }
            // Orphan guard: reconnect every surviving out-neighbour whose sole
            // live incoming edge was `id -> out_nb`.
            for &out_nb in &surviving {
                if !self.rev_edges[out_nb as usize][d].is_empty() {
                    continue;
                }
                let mut candidates: Vec<(f64, u32)> = live_ins
                    .iter()
                    .copied()
                    .filter(|&in_nb| in_nb != out_nb)
                    .map(|in_nb| (self.dist(in_nb, out_nb), in_nb))
                    .collect();
                candidates.sort_by(|a, b| {
                    OrderedFloat(a.0)
                        .cmp(&OrderedFloat(b.0))
                        .then_with(|| a.1.cmp(&b.1))
                });
                let mut reconnected = false;
                for (dist, in_nb) in candidates {
                    let list = &mut self.towers[in_nb as usize].get_mut().unwrap()[d];
                    if list.iter().any(|&(dst, _)| dst == out_nb) {
                        reconnected = true;
                        break;
                    }
                    if list.len() < m_max {
                        list.push((out_nb, dist));
                        self.rev_edges[out_nb as usize][d].push(in_nb);
                        reconnected = true;
                        break;
                    }
                    // At cap: swap out the farthest replaceable edge whose target
                    // retains another incoming edge on this layer.
                    let swap_pos = list
                        .iter()
                        .enumerate()
                        .filter(|(_, &(old_dst, _))| {
                            self.rev_edges[old_dst as usize][d].len() > 1
                        })
                        .max_by(|(_, a), (_, b)| {
                            OrderedFloat(a.1)
                                .cmp(&OrderedFloat(b.1))
                                .then_with(|| a.0.cmp(&b.0))
                        })
                        .map(|(pos, _)| pos);
                    if let Some(pos) = swap_pos {
                        let (old_dst, _) = list[pos];
                        list[pos] = (out_nb, dist);
                        self.rev_edges[old_dst as usize][d].retain(|&u| u != in_nb);
                        self.rev_edges[out_nb as usize][d].push(in_nb);
                        reconnected = true;
                        break;
                    }
                }
                if !reconnected {
                    crate::runtime::diagnostics::emit(
                        "hnsw.repair_fallback",
                        format!(
                            "HNSW in-RAM catch-up removal on relation '{relation}' (layer {}) \
                             could not reconnect orphaned neighbour within degree cap {m_max}",
                            -(d as i64)
                        ),
                        format!(
                            "Run `::reindex {relation}` during maintenance if full ANN \
                             connectivity needs to be restored."
                        ),
                    );
                }
            }
            // Also keep any live in-neighbour whose outgoing list became empty
            // attached to the nearest surviving out-neighbour.
            for &in_nb in &live_ins {
                let is_empty = self.towers[in_nb as usize].get_mut().unwrap()[d].is_empty();
                if is_empty {
                    if let Some((dist, alt)) = surviving
                        .iter()
                        .copied()
                        .filter(|&s| s != in_nb)
                        .map(|s| (self.dist(in_nb, s), s))
                        .min_by(|a, b| {
                            OrderedFloat(a.0)
                                .cmp(&OrderedFloat(b.0))
                                .then_with(|| a.1.cmp(&b.1))
                        })
                    {
                        self.towers[in_nb as usize].get_mut().unwrap()[d].push((alt, dist));
                        self.rev_edges[alt as usize][d].push(in_nb);
                    }
                }
            }
        }
        if self.entry.get_mut().unwrap().is_some_and(|(ep, _)| ep == id) {
            let mut best: Option<(u32, i64)> = None;
            for (i, &is_del) in self.deleted.iter().enumerate() {
                if is_del {
                    continue;
                }
                let lvl = self.levels[i];
                if best.is_none_or(|(_, top)| lvl < top) {
                    best = Some((i as u32, lvl));
                }
            }
            *self.entry.get_mut().unwrap() = best;
        }
    }

    /// Insert a new node into an already-built in-RAM graph during catch-up,
    /// keeping `rev_edges` synchronized and preserving the 0.19.2 insertion
    /// orphan guard (never leaving the inserted node or a pruned neighbour
    /// without an incoming edge on layer `d`).
    pub(crate) fn insert_dynamic(
        &mut self,
        meta: NodeMeta,
        vec: &Vector,
        manifest: &HnswIndexManifest,
        relation: &str,
    ) -> Result<u32> {
        if vec.len() != self.dim {
            bail!(
                "vector dimension mismatch during HNSW catch-up: expected {}, got {}",
                self.dim,
                vec.len()
            );
        }
        match (&mut self.slab, vec) {
            (VecSlab::F32(s), Vector::F32(v)) => s.extend(v.iter()),
            (VecSlab::F64(s), Vector::F64(v)) => s.extend(v.iter()),
            _ => bail!("vector element type does not match the index dtype"),
        }
        let id = self.metas.len() as u32;
        self.metas.push(meta);
        if matches!(self.distance, HnswDistance::Cosine) {
            let norm = self.slab.self_dot(id as usize, self.dim);
            self.norms_sq.push(norm);
        }
        let target_level = manifest.get_random_level();
        self.levels.push(target_level);
        self.towers
            .push(Mutex::new(vec![vec![]; depth_of(target_level) + 1]));
        self.rev_edges.push(vec![Vec::new(); depth_of(target_level) + 1]);
        self.deleted.push(false);

        let Some((ep_id, ep_top)) = *self.entry.get_mut().unwrap() else {
            *self.entry.get_mut().unwrap() = Some((id, target_level));
            return Ok(id);
        };

        let mut bufs = SearchBuffers::default();
        bufs.found.push(ep_id, OrderedFloat(self.dist(id, ep_id)));
        for level in ep_top..target_level {
            self.search_level(id, 1, level, &mut bufs);
        }
        for level in max(target_level, ep_top)..=0 {
            let m_max = if level == 0 { self.m_max0 } else { self.m_max };
            let m_select = self.m_neighbours.min(m_max);
            let d = depth_of(level);
            self.search_level(id, self.ef_construction, level, &mut bufs);
            let neighbours = self.select_heuristic(
                id,
                m_select,
                level,
                self.extend_candidates,
                &bufs.found,
                &mut bufs.sel_candidates,
                &mut bufs.sel_discarded,
                &mut bufs.neigh,
            );
            {
                let tower = self.towers[id as usize].get_mut().unwrap();
                tower[d] = neighbours.clone();
            }
            let mut kept_in_links = 0usize;
            for &(nb, dist) in &neighbours {
                self.rev_edges[nb as usize][d].push(id);
                let maybe_overflow = {
                    let list = &mut self.towers[nb as usize].get_mut().unwrap()[d];
                    if !list.iter().any(|&(e, _)| e == id) {
                        list.push((id, dist));
                    }
                    if list.len() > m_max {
                        Some(list.clone())
                    } else {
                        None
                    }
                };
                if let Some(old_list) = maybe_overflow {
                    let mut kept = self.select_from_list(&old_list, m_max);
                    // Protect any existing neighbour whose sole incoming edge
                    // on this layer is `nb -> old_dst`.
                    for &(old_dst, old_dist) in &old_list {
                        if old_dst == id || kept.iter().any(|&(e, _)| e == old_dst) {
                            continue;
                        }
                        if self.rev_edges[old_dst as usize][d].len() <= 1 {
                            let swap_pos = kept
                                .iter()
                                .enumerate()
                                .filter(|(_, &(k, _))| {
                                    k == id || self.rev_edges[k as usize][d].len() > 1
                                })
                                .max_by(|(_, a), (_, b)| {
                                    OrderedFloat(a.1)
                                        .cmp(&OrderedFloat(b.1))
                                        .then_with(|| a.0.cmp(&b.0))
                                })
                                .map(|(pos, _)| pos);
                            if let Some(pos) = swap_pos {
                                kept[pos] = (old_dst, old_dist);
                            }
                        }
                    }
                    if kept.iter().any(|&(e, _)| e == id) {
                        self.rev_edges[id as usize][d].push(nb);
                        kept_in_links += 1;
                    }
                    for (old_dst, _) in old_list {
                        if old_dst != id && !kept.iter().any(|&(e, _)| e == old_dst) {
                            self.rev_edges[old_dst as usize][d].retain(|&u| u != nb);
                        }
                    }
                    self.towers[nb as usize].get_mut().unwrap()[d] = kept;
                } else {
                    self.rev_edges[id as usize][d].push(nb);
                    kept_in_links += 1;
                }
            }
            // Insertion orphan guard (0.19.2): if every selected neighbour was
            // at cap and pruned its link back to `id`, restore one incoming
            // link from the nearest neighbour with a replaceable edge so `id`
            // stays reachable on this layer.
            if kept_in_links == 0 && !neighbours.is_empty() {
                let mut by_dist = neighbours.clone();
                by_dist.sort_by(|a, b| {
                    OrderedFloat(a.1)
                        .cmp(&OrderedFloat(b.1))
                        .then_with(|| a.0.cmp(&b.0))
                });
                let mut attached = false;
                for (nb, dist) in by_dist {
                    let list = &mut self.towers[nb as usize].get_mut().unwrap()[d];
                    if list.len() < m_max {
                        list.push((id, dist));
                        self.rev_edges[id as usize][d].push(nb);
                        attached = true;
                        break;
                    }
                    let swap_pos = list
                        .iter()
                        .enumerate()
                        .filter(|(_, &(old_dst, _))| {
                            self.rev_edges[old_dst as usize][d].len() > 1
                        })
                        .max_by(|(_, a), (_, b)| {
                            OrderedFloat(a.1)
                                .cmp(&OrderedFloat(b.1))
                                .then_with(|| a.0.cmp(&b.0))
                        })
                        .map(|(pos, _)| pos);
                    if let Some(pos) = swap_pos {
                        let (old_dst, _) = list[pos];
                        list[pos] = (id, dist);
                        self.rev_edges[old_dst as usize][d].retain(|&u| u != nb);
                        self.rev_edges[id as usize][d].push(nb);
                        attached = true;
                        break;
                    }
                }
                if !attached {
                    crate::runtime::diagnostics::emit(
                        "hnsw.repair_fallback",
                        format!(
                            "HNSW in-RAM catch-up insertion on relation '{relation}' (layer \
                             {level}) could not restore an incoming link within degree cap {m_max}"
                        ),
                        format!(
                            "Run `::reindex {relation}` during maintenance if full ANN \
                             connectivity needs to be restored."
                        ),
                    );
                }
            }
        }
        if target_level < ep_top {
            *self.entry.get_mut().unwrap() = Some((id, target_level));
        }
        Ok(id)
    }

    pub(crate) fn into_graph(self) -> FlatHnswGraph {
        let entry = *self.entry.read().unwrap();
        let towers: Vec<Vec<Vec<(u32, f64)>>> = self
            .towers
            .into_iter()
            .map(|m| m.into_inner().unwrap())
            .collect();
        let deleted = self.deleted;

        let mut dead: Vec<Vec<Vec<(u32, f64)>>> =
            towers.iter().map(|t| vec![Vec::new(); t.len()]).collect();
        for (i, tower) in towers.iter().enumerate() {
            if deleted.get(i).copied().unwrap_or(false) {
                continue;
            }
            for (depth, list) in tower.iter().enumerate() {
                for &(j, dist) in list {
                    debug_assert!(!deleted.get(j as usize).copied().unwrap_or(false));
                    let linked_back = towers[j as usize]
                        .get(depth)
                        .map(|back| back.iter().any(|&(other, _)| other == i as u32))
                        .unwrap_or(true);
                    if !linked_back {
                        dead[j as usize][depth].push((i as u32, dist));
                    }
                }
            }
        }
        FlatHnswGraph {
            metas: self.metas,
            towers,
            dead,
            deleted,
            entry,
        }
    }

    /// Build the graph. `manifest.get_random_level()` semantics are preserved
    /// by drawing each node's level up front from the same distribution.
    pub(crate) fn build(mut self, manifest: &HnswIndexManifest, threads: usize) -> FlatHnswGraph {
        self.build_graph(manifest, threads);
        self.into_graph()
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
            let m_select = self.m_neighbours.min(m_max);
            self.search_level(q, self.ef_construction, level, bufs);
            let neighbours = self.select_heuristic(
                q,
                m_select,
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
            if distance_is_farther(cand_dist, *furthest) {
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
                if bufs.found.len() < ef || distance_is_closer(nb_dist, *furthest) {
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
                if distance_is_closer(self.dist(existing, cand), cand_dist) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::value::{DataValue, Vector};
    use ndarray::Array1;

    fn test_manifest() -> HnswIndexManifest {
        HnswIndexManifest {
            base_relation: "rel".into(),
            index_name: "idx".into(),
            vec_dim: 8,
            dtype: VecElementType::F32,
            vec_fields: vec![1],
            distance: HnswDistance::Cosine,
            ef_construction: 32,
            m_neighbours: 4,
            m_max: 4,
            m_max0: 8,
            level_multiplier: 1.0 / (4.0f64).ln(),
            index_filter: None,
            extend_candidates: false,
            keep_pruned_connections: false,
        }
    }

    fn mk_vec(seed: usize) -> Vector {
        let vals: Vec<f32> = (0..8)
            .map(|d| (((seed * 31 + d * 17) % 100) as f32 + 1.0) / 100.0)
            .collect();
        Vector::F32(Array1::from(vals))
    }

    #[test]
    fn incremental_remove_and_insert_preserves_invariants() {
        let manifest = test_manifest();
        let mut builder = FlatHnswBuilder::new(&manifest);
        let mut ids = Vec::new();
        for i in 0..40usize {
            let v = mk_vec(i);
            let id = builder
                .add_node(
                    NodeMeta {
                        key: vec![DataValue::from(i as i64)],
                        field_idx: 1,
                        sub_idx: -1,
                        hash: v.get_hash().as_ref().to_vec(),
                    },
                    &v,
                )
                .unwrap();
            ids.push(id);
        }
        builder.build_graph(&manifest, 2);
        builder.prepare_incremental();
        // Remove every 4th node (including possible entry point) and insert replacements.
        for i in (0..40).step_by(4) {
            builder.remove_node(ids[i], "rel");
        }
        for i in 40..50usize {
            let v = mk_vec(i);
            builder
                .insert_dynamic(
                    NodeMeta {
                        key: vec![DataValue::from(i as i64)],
                        field_idx: 1,
                        sub_idx: -1,
                        hash: v.get_hash().as_ref().to_vec(),
                    },
                    &v,
                    &manifest,
                    "rel",
                )
                .unwrap();
        }
        let graph = builder.into_graph();
        let (ep, _) = graph.entry.expect("non-empty graph must have entry");
        assert!(
            !graph.deleted[ep as usize],
            "entry point must not be deleted"
        );
        for i in 0..graph.metas.len() {
            if graph.deleted[i] {
                continue;
            }
            for (d, live) in graph.towers[i].iter().enumerate() {
                for &(nb, _) in live {
                    assert!(
                        !graph.deleted[nb as usize],
                        "live edge {i} -> {nb} points to deleted node"
                    );
                    let rev_live = graph.towers[nb as usize][d]
                        .iter()
                        .any(|&(u, _)| u == i as u32);
                    let rev_dead = graph.dead[nb as usize][d]
                        .iter()
                        .any(|&(u, _)| u == i as u32);
                    assert!(
                        rev_live || rev_dead,
                        "live edge {i} -> {nb} at depth {d} missing paired reverse entry"
                    );
                }
                for &(nb, _) in &graph.dead[i][d] {
                    assert!(
                        !graph.deleted[nb as usize],
                        "dead reverse edge {i} -> {nb} points to deleted node"
                    );
                }
            }
        }
        // Every surviving node on layer 0 must retain at least one live incoming edge.
        let mut has_in0 = vec![false; graph.metas.len()];
        for i in 0..graph.metas.len() {
            if graph.deleted[i] {
                continue;
            }
            for &(nb, _) in &graph.towers[i][0] {
                has_in0[nb as usize] = true;
            }
        }
        for (i, &has_in) in has_in0.iter().enumerate() {
            if !graph.deleted[i] {
                assert!(has_in, "surviving node {i} orphaned on layer 0");
            }
        }
    }

    #[test]
    fn remove_node_reconnects_sole_in_edge_out_neighbour_even_when_in_nb_non_empty() {
        let mut manifest = test_manifest();
        manifest.m_neighbours = 2;
        manifest.m_max = 2;
        manifest.m_max0 = 4;
        let mut builder = FlatHnswBuilder::new(&manifest);
        // 4 nodes: A(0), B(1), C(2), D(3).
        // Hand-wire layer 0: A -> B, A -> C, B -> D (D's sole incoming edge), C -> A.
        for i in 0..4usize {
            let v = mk_vec(i + 1);
            builder
                .add_node(
                    NodeMeta {
                        key: vec![DataValue::from(i as i64)],
                        field_idx: 1,
                        sub_idx: -1,
                        hash: v.get_hash().as_ref().to_vec(),
                    },
                    &v,
                )
                .unwrap();
        }
        builder.build_graph(&manifest, 1);
        builder.levels = vec![0, 0, 0, 0];
        builder.deleted = vec![false; 4];
        builder.towers = vec![
            Mutex::new(vec![vec![(1, 0.1), (2, 0.2)]]), // A -> B, C
            Mutex::new(vec![vec![(3, 0.1)]]),           // B -> D
            Mutex::new(vec![vec![(0, 0.2)]]),           // C -> A
            Mutex::new(vec![vec![(2, 0.3)]]),           // D -> C
        ];
        *builder.entry.get_mut().unwrap() = Some((0, 0));
        builder.prepare_incremental();
        builder.remove_node(1, "rel"); // remove B; A still has A -> C, D must be reconnected!
        let graph = builder.into_graph();
        assert!(
            graph.towers[0][0].iter().any(|&(dst, _)| dst == 3),
            "removing bridge B must reconnect A -> D so D is not orphaned"
        );
    }
}
