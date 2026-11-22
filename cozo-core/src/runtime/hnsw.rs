/*
 * Copyright 2023, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use crate::data::expr::{eval_bytecode_pred, Bytecode};
use crate::data::program::HnswSearch;
use crate::data::relation::VecElementType;
use crate::data::tuple::Tuple;
use crate::data::value::Vector;
use crate::parse::expr::build_expr;
use crate::parse::sys::HnswDistance;
use crate::parse::{CozoScriptParser, Rule};
use crate::runtime::relation::{try_decode_val_only, RelationHandle};
use crate::runtime::transact::SessionTx;
use crate::storage::StoreTx;
use crate::{DataValue, SourceSpan};
use itertools::Itertools;
use miette::{bail, miette, IntoDiagnostic, Result};
use ordered_float::OrderedFloat;
use pest::Parser;
use priority_queue::PriorityQueue;
use rand::Rng;
use rustc_hash::{FxHashMap, FxHashSet};
use smartstring::{LazyCompact, SmartString};
use std::cmp::{max, Reverse};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, serde_derive::Serialize, serde_derive::Deserialize)]
pub(crate) struct HnswIndexManifest {
    pub(crate) base_relation: SmartString<LazyCompact>,
    pub(crate) index_name: SmartString<LazyCompact>,
    pub(crate) vec_dim: usize,
    pub(crate) dtype: VecElementType,
    pub(crate) vec_fields: Vec<usize>,
    pub(crate) distance: HnswDistance,
    pub(crate) ef_construction: usize,
    pub(crate) m_neighbours: usize,
    pub(crate) m_max: usize,
    pub(crate) m_max0: usize,
    pub(crate) level_multiplier: f64,
    pub(crate) index_filter: Option<String>,
    pub(crate) extend_candidates: bool,
    pub(crate) keep_pruned_connections: bool,
}

impl HnswIndexManifest {
    pub(crate) fn get_random_level(&self) -> i64 {
        let mut rng = rand::thread_rng();
        let uniform_num: f64 = rng.gen_range(0.0..1.0);
        let r = -uniform_num.ln() * self.level_multiplier;
        // the level is the largest integer smaller than r
        -(r.floor() as i64)
    }
}

mod repair;
pub(crate) use repair::PrecomputedLayerRemoval;

pub(crate) type CompoundKey = (Tuple, usize, i32);

#[inline]
pub(crate) fn cosine_distance(a_norm_sq: f64, b_norm_sq: f64, dot: f64) -> f64 {
    let denominator = (a_norm_sq * b_norm_sq).sqrt();
    if denominator > 0.0 {
        1.0 - dot / denominator
    } else {
        // A zero/invalid embedding must be maximally distant, finite, and JSON-safe.
        2.0
    }
}

#[inline]
pub(crate) fn distance_is_closer(candidate: f64, current_furthest: f64) -> bool {
    OrderedFloat(candidate) < OrderedFloat(current_furthest)
}

#[inline]
pub(crate) fn distance_is_farther(candidate: f64, current_furthest: f64) -> bool {
    OrderedFloat(candidate) > OrderedFloat(current_furthest)
}

#[cfg(test)]
mod distance_order_tests {
    use super::{distance_is_closer, distance_is_farther};

    #[test]
    fn heap_comparisons_use_one_total_order_for_nan() {
        assert!(distance_is_closer(0.25, f64::NAN));
        assert!(distance_is_farther(f64::NAN, 0.25));
        assert!(!distance_is_closer(f64::NAN, 0.25));
        assert!(!distance_is_farther(0.25, f64::NAN));
    }
}

pub(crate) struct VectorCache<'r, 's> {
    pub(crate) cache: FxHashMap<CompoundKey, Vector>,
    pub(crate) distance: HnswDistance,
    pub(crate) base_tx: Option<&'r SessionTx<'s>>,
    pub(crate) shadow: Option<(crate::runtime::txtime_index::SharedBuildRegistry, u64)>,
    has_list_vectors: bool,
}

impl VectorCache<'static, 'static> {
    pub(crate) fn new(distance: HnswDistance) -> Self {
        Self {
            cache: FxHashMap::default(),
            distance,
            base_tx: None,
            shadow: None,
            has_list_vectors: false,
        }
    }
    pub(crate) fn with_shadow(
        distance: HnswDistance,
        shared: crate::runtime::txtime_index::SharedBuildRegistry,
        token: u64,
    ) -> Self {
        Self {
            cache: FxHashMap::default(),
            distance,
            base_tx: None,
            shadow: Some((shared, token)),
            has_list_vectors: false,
        }
    }
}

impl<'r, 's> VectorCache<'r, 's> {
    pub(crate) fn with_base_tx(distance: HnswDistance, base_tx: &'r SessionTx<'s>) -> Self {
        Self {
            cache: FxHashMap::default(),
            distance,
            base_tx: Some(base_tx),
            shadow: None,
            has_list_vectors: false,
        }
    }
    pub(crate) fn clear(&mut self) {
        self.cache.clear();
        self.has_list_vectors = false;
    }
    pub(crate) fn seed_row(
        &mut self,
        manifest: &HnswIndexManifest,
        orig: &RelationHandle,
        row: &Tuple,
    ) {
        let key = row[..orig.index_key_len()].to_vec();
        for &field in &manifest.vec_fields {
            match &row[field] {
                DataValue::Vec(v) => {
                    self.insert((key.clone(), field, -1), v.clone());
                }
                DataValue::List(values) => {
                    for (sub, value) in values.iter().enumerate() {
                        if let DataValue::Vec(v) = value {
                            self.insert((key.clone(), field, sub as i32), v.clone());
                        }
                    }
                }
                _ => {}
            }
        }
    }
    pub(crate) fn evict_row(
        &mut self,
        manifest: &HnswIndexManifest,
        orig: &RelationHandle,
        row: &Tuple,
    ) {
        let key = row[..orig.index_key_len()].to_vec();
        for &field in &manifest.vec_fields {
            match &row[field] {
                DataValue::Vec(_) => {
                    self.cache.remove(&(key.clone(), field, -1));
                }
                DataValue::List(values) => {
                    for sub in 0..values.len() {
                        self.cache.remove(&(key.clone(), field, sub as i32));
                    }
                }
                _ => {}
            }
        }
    }
    pub(crate) fn evict_logical_keys(
        &mut self,
        manifest: &HnswIndexManifest,
        keys: &FxHashSet<Tuple>,
    ) {
        if keys.is_empty() || self.cache.is_empty() {
            return;
        }
        if !self.has_list_vectors {
            for logical_key in keys {
                for &field in &manifest.vec_fields {
                    self.cache.remove(&(logical_key.clone(), field, -1));
                }
            }
        } else {
            self.cache.retain(|(k, _, _), _| !keys.contains(k));
        }
    }
    fn insert(&mut self, k: CompoundKey, v: Vector) -> Option<Vector> {
        if k.2 >= 0 {
            self.has_list_vectors = true;
        }
        self.cache.insert(k, v)
    }
    #[inline]
    pub(super) fn vec_norm_sq(&self, v: &Vector) -> f64 {
        match self.distance {
            HnswDistance::Cosine => match v {
                Vector::F32(a) => a
                    .as_slice()
                    .map(|s| s.iter().map(|&x| (x as f64) * (x as f64)).sum())
                    .unwrap_or_else(|| a.dot(a) as f64),
                Vector::F64(a) => a
                    .as_slice()
                    .map(|s| s.iter().map(|&x| x * x).sum())
                    .unwrap_or_else(|| a.dot(a)),
            },
            _ => 0.0,
        }
    }
    fn dist(&self, v1: &Vector, v2: &Vector) -> f64 {
        let q_norm_sq = self.vec_norm_sq(v1);
        self.dist_with_norm(v1, q_norm_sq, v2)
    }
    #[inline]
    pub(super) fn dist_with_both_norms(
        &self,
        v1: &Vector,
        v1_norm_sq: f64,
        v2: &Vector,
        v2_norm_sq: f64,
    ) -> f64 {
        match self.distance {
            HnswDistance::Cosine => match (v1, v2) {
                (Vector::F32(a), Vector::F32(b)) => {
                    if let (Some(sa), Some(sb)) = (a.as_slice(), b.as_slice()) {
                        if sa.len() == sb.len() {
                            let mut dot = 0.0f64;
                            for (&x, &y) in sa.iter().zip(sb.iter()) {
                                dot += (x as f64) * (y as f64);
                            }
                            return cosine_distance(v1_norm_sq, v2_norm_sq, dot);
                        }
                    }
                    cosine_distance(v1_norm_sq, v2_norm_sq, a.dot(b) as f64)
                }
                (Vector::F64(a), Vector::F64(b)) => {
                    if let (Some(sa), Some(sb)) = (a.as_slice(), b.as_slice()) {
                        if sa.len() == sb.len() {
                            let mut dot = 0.0f64;
                            for (&x, &y) in sa.iter().zip(sb.iter()) {
                                dot += x * y;
                            }
                            return cosine_distance(v1_norm_sq, v2_norm_sq, dot);
                        }
                    }
                    cosine_distance(v1_norm_sq, v2_norm_sq, a.dot(b))
                }
                _ => panic!(
                    "Cannot compute cosine distance between {:?} and {:?}",
                    v1, v2
                ),
            },
            _ => self.dist_with_norm(v1, v1_norm_sq, v2),
        }
    }
    #[inline]
    fn dist_with_norm(&self, v1: &Vector, v1_norm_sq: f64, v2: &Vector) -> f64 {
        match self.distance {
            HnswDistance::L2 => match (v1, v2) {
                (Vector::F32(a), Vector::F32(b)) => {
                    if let (Some(sa), Some(sb)) = (a.as_slice(), b.as_slice()) {
                        if sa.len() == sb.len() {
                            return sa
                                .iter()
                                .zip(sb.iter())
                                .map(|(&x, &y)| {
                                    let d = x - y;
                                    d * d
                                })
                                .sum::<f32>() as f64;
                        }
                    }
                    let diff = a - b;
                    diff.dot(&diff) as f64
                }
                (Vector::F64(a), Vector::F64(b)) => {
                    if let (Some(sa), Some(sb)) = (a.as_slice(), b.as_slice()) {
                        if sa.len() == sb.len() {
                            return sa
                                .iter()
                                .zip(sb.iter())
                                .map(|(&x, &y)| {
                                    let d = x - y;
                                    d * d
                                })
                                .sum();
                        }
                    }
                    let diff = a - b;
                    diff.dot(&diff)
                }
                _ => panic!("Cannot compute L2 distance between {:?} and {:?}", v1, v2),
            },
            HnswDistance::Cosine => match (v1, v2) {
                (Vector::F32(a), Vector::F32(b)) => {
                    if let (Some(sa), Some(sb)) = (a.as_slice(), b.as_slice()) {
                        if sa.len() == sb.len() {
                            let mut dot = 0.0f64;
                            let mut b_norm = 0.0f64;
                            for (&x, &y) in sa.iter().zip(sb.iter()) {
                                let xf = x as f64;
                                let yf = y as f64;
                                dot += xf * yf;
                                b_norm += yf * yf;
                            }
                            return cosine_distance(v1_norm_sq, b_norm, dot);
                        }
                    }
                    let b_norm = b.dot(b) as f64;
                    let dot = a.dot(b) as f64;
                    cosine_distance(v1_norm_sq, b_norm, dot)
                }
                (Vector::F64(a), Vector::F64(b)) => {
                    if let (Some(sa), Some(sb)) = (a.as_slice(), b.as_slice()) {
                        if sa.len() == sb.len() {
                            let mut dot = 0.0f64;
                            let mut b_norm = 0.0f64;
                            for (&x, &y) in sa.iter().zip(sb.iter()) {
                                dot += x * y;
                                b_norm += y * y;
                            }
                            return cosine_distance(v1_norm_sq, b_norm, dot);
                        }
                    }
                    let b_norm = b.dot(b);
                    let dot = a.dot(b);
                    cosine_distance(v1_norm_sq, b_norm, dot)
                }
                _ => panic!(
                    "Cannot compute cosine distance between {:?} and {:?}",
                    v1, v2
                ),
            },
            HnswDistance::InnerProduct => match (v1, v2) {
                (Vector::F32(a), Vector::F32(b)) => {
                    let dot = if let (Some(sa), Some(sb)) = (a.as_slice(), b.as_slice()) {
                        if sa.len() == sb.len() {
                            sa.iter().zip(sb.iter()).map(|(&x, &y)| x * y).sum::<f32>()
                        } else {
                            a.dot(b)
                        }
                    } else {
                        a.dot(b)
                    };
                    1. - dot as f64
                }
                (Vector::F64(a), Vector::F64(b)) => {
                    let dot = if let (Some(sa), Some(sb)) = (a.as_slice(), b.as_slice()) {
                        if sa.len() == sb.len() {
                            sa.iter().zip(sb.iter()).map(|(&x, &y)| x * y).sum::<f64>()
                        } else {
                            a.dot(b)
                        }
                    } else {
                        a.dot(b)
                    };
                    1. - dot
                }
                _ => panic!("Cannot compute inner product between {:?} and {:?}", v1, v2),
            },
        }
    }
    fn v_dist(&self, v: &Vector, key: &CompoundKey) -> f64 {
        let v2 = self.cache.get(key).unwrap();
        self.dist(v, v2)
    }
    #[inline]
    fn v_dist_with_norm(&self, v: &Vector, v_norm_sq: f64, key: &CompoundKey) -> f64 {
        let v2 = self.cache.get(key).unwrap();
        self.dist_with_norm(v, v_norm_sq, v2)
    }
    fn k_dist(&self, k1: &CompoundKey, k2: &CompoundKey) -> f64 {
        let v1 = self.cache.get(k1).unwrap();
        let v2 = self.cache.get(k2).unwrap();
        self.dist(v1, v2)
    }
    fn get_key(&self, key: &CompoundKey) -> &Vector {
        self.cache.get(key).unwrap()
    }
    fn ensure_key(
        &mut self,
        key: &CompoundKey,
        handle: &RelationHandle,
        tx: &SessionTx<'_>,
    ) -> Result<()> {
        if !self.cache.contains_key(key) {
            let found = if let Some((ref shared, token)) = self.shadow {
                let encoded = crate::data::tuple::TupleT::encode_as_key(&key.0, handle.id);
                match crate::runtime::txtime_index::lookup_shadow_pending(shared, token, &encoded)?
                {
                    Some(opt_tuple) => opt_tuple,
                    None => handle.get_current(tx, &key.0)?,
                }
            } else {
                match self.base_tx {
                    Some(base_tx) => handle.get_current(base_tx, &key.0)?,
                    None => handle.get_current(tx, &key.0)?,
                }
            };
            match found {
                Some(tuple) => self.insert_from_tuple(key, &tuple)?,
                None => bail!("Cannot find compound key for HNSW: {:?}", key),
            }
        }
        Ok(())
    }

    /// Batch `ensure_key` (mnestic fork): one storage `multi_get` covers every
    /// not-yet-cached key. Used on the search hot path, where serial
    /// per-neighbour point gets were measured as a large share of
    /// disk-resident HNSW query cost.
    fn ensure_keys(
        &mut self,
        keys: &[CompoundKey],
        handle: &RelationHandle,
        tx: &SessionTx<'_>,
    ) -> Result<()> {
        let missing: Vec<&CompoundKey> = keys
            .iter()
            .filter(|k| !self.cache.contains_key(*k))
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        if missing.len() == 1 {
            return self.ensure_key(missing[0], handle, tx);
        }
        if let Some((ref shared, token)) = self.shadow {
            let encoded_keys: Vec<Vec<u8>> = missing
                .iter()
                .map(|k| crate::data::tuple::TupleT::encode_as_key(&k.0, handle.id))
                .collect();
            let overrides = crate::runtime::txtime_index::lookup_shadow_pending_batch(
                shared,
                token,
                &encoded_keys,
            )?;
            let mut from_db: Vec<&CompoundKey> = Vec::new();
            for (key, ov) in missing.into_iter().zip(overrides) {
                match ov {
                    Some(Some(tuple)) => self.insert_from_tuple(key, &tuple)?,
                    Some(None) => bail!("Cannot find compound key for HNSW: {:?}", key),
                    None => from_db.push(key),
                }
            }
            if from_db.is_empty() {
                return Ok(());
            }
            let key_slices: Vec<&[DataValue]> = from_db.iter().map(|k| k.0.as_slice()).collect();
            let tuples = handle.get_current_batch(tx, &key_slices)?;
            for (key, tuple) in from_db.into_iter().zip(tuples) {
                match tuple {
                    Some(tuple) => self.insert_from_tuple(key, &tuple)?,
                    None => bail!("Cannot find compound key for HNSW: {:?}", key),
                }
            }
            return Ok(());
        }
        let key_slices: Vec<&[DataValue]> = missing.iter().map(|k| k.0.as_slice()).collect();
        let tuples = match self.base_tx {
            Some(base_tx) => handle.get_current_batch(base_tx, &key_slices)?,
            None => handle.get_current_batch(tx, &key_slices)?,
        };
        for (key, tuple) in missing.iter().zip(tuples) {
            match tuple {
                Some(tuple) => self.insert_from_tuple(key, &tuple)?,
                None => bail!("Cannot find compound key for HNSW: {:?}", key),
            }
        }
        Ok(())
    }

    fn insert_from_tuple(&mut self, key: &CompoundKey, tuple: &[DataValue]) -> Result<()> {
        let mut field = tuple
            .get(key.1)
            .ok_or_else(|| miette!("corrupt HNSW vector field"))?;
        if key.2 >= 0 {
            match field {
                DataValue::List(l) => {
                    field = l
                        .get(key.2 as usize)
                        .ok_or_else(|| miette!("corrupt HNSW vector subindex"))?;
                }
                _ => bail!("Cannot interpret {} as list", field),
            }
        }
        match field {
            DataValue::Vec(v) => {
                self.insert(key.clone(), v.clone());
            }
            _ => bail!("Cannot interpret {} as vector", field),
        }
        Ok(())
    }
}

impl<'a> SessionTx<'a> {
    // --- index-store routing (mnestic fork) -------------------------------
    // During `create_hnsw_index` the index handle is marked `is_temp` so the
    // whole graph is built in the in-RAM temp store (a plain BTreeMap), instead
    // of round-tripping every neighbour read/write through the pessimistic
    // transaction's RocksDB `WriteBatchWithIndex` overlay (which grows with the
    // index and makes the build superlinear). These helpers route raw index
    // KV ops to the same store the `RelationHandle` read methods already pick
    // via `is_temp`, so build and steady-state writes stay byte-identical.
    #[inline]
    fn idx_put(&mut self, idx_table: &RelationHandle, key: &[u8], val: &[u8]) -> Result<()> {
        if idx_table.is_temp {
            self.temp_store_tx.put(key, val)
        } else {
            self.store_tx.put(key, val)
        }
    }
    #[inline]
    fn idx_get(&self, idx_table: &RelationHandle, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if idx_table.is_temp {
            self.temp_store_tx.get(key, false)
        } else {
            self.store_tx.get(key, false)
        }
    }
    #[inline]
    fn idx_del(&mut self, idx_table: &RelationHandle, key: &[u8]) -> Result<()> {
        if idx_table.is_temp {
            self.temp_store_tx.del(key)
        } else {
            self.store_tx.del(key)
        }
    }
    #[inline]
    fn idx_exists(&self, idx_table: &RelationHandle, key: &[u8]) -> Result<bool> {
        if idx_table.is_temp {
            self.temp_store_tx.exists(key, false)
        } else {
            self.store_tx.exists(key, false)
        }
    }
}

/// Pre-computed shrink heuristic + 2-hop witness for a full-degree neighbour,
/// computed on a read snapshot outside `tt_commit_lock` and verified for exact
/// candidate-set equality and witness liveness inside `tt_commit_lock`.
pub(crate) struct PrecomputedShrink {
    pub(crate) expected_existing: Vec<CompoundKey>,
    pub(crate) heuristic: Vec<(CompoundKey, f64)>,
    pub(crate) fast_drop: Option<(CompoundKey, CompoundKey)>,
}

/// Pre-computed HNSW candidate search and tentative heuristic selection for a
/// single layer `level` in `target_level..=0`.
pub(crate) struct PresearchedLevel {
    pub(crate) level: i64,
    pub(crate) candidates: PriorityQueue<CompoundKey, OrderedFloat<f64>>,
    pub(crate) tentative_neighbours:
        Option<PriorityQueue<CompoundKey, Reverse<OrderedFloat<f64>>>>,
}

/// Pre-computed multi-level HNSW candidate search, tentative neighbour
/// selections, and tentative neighbour shrink heuristics produced on a read
/// snapshot outside `tt_commit_lock` so concurrent writers run read-only ANN
/// descent, vector pre-warming, and heuristic distance loops in parallel.
pub(crate) struct HnswPresearch {
    pub(crate) target_level: i64,
    pub(crate) levels: Vec<PresearchedLevel>,
    pub(crate) shrinks: FxHashMap<(i64, CompoundKey), PrecomputedShrink>,
}

impl HnswPresearch {
    pub(crate) fn prune_keys(&mut self, mutated: &FxHashSet<Tuple>, self_key: &[DataValue]) {
        for lvl in &mut self.levels {
            let drop_keys: Vec<CompoundKey> = lvl
                .candidates
                .iter()
                .filter(|(k, _)| k.0.as_slice() == self_key || mutated.contains(&k.0))
                .map(|(k, _)| k.clone())
                .collect();
            if !drop_keys.is_empty() {
                for k in drop_keys {
                    lvl.candidates.remove(&k);
                }
                lvl.tentative_neighbours = None;
            } else if lvl.tentative_neighbours.as_ref().is_some_and(|tn| {
                tn.iter()
                    .any(|(k, _)| k.0.as_slice() == self_key || mutated.contains(&k.0))
            }) {
                lvl.tentative_neighbours = None;
            }
        }
        self.shrinks.retain(|(_, target), shrink| {
            target.0.as_slice() != self_key
                && !mutated.contains(&target.0)
                && shrink
                    .expected_existing
                    .iter()
                    .all(|k| k.0.as_slice() != self_key && !mutated.contains(&k.0))
        });
    }

    pub(crate) fn is_usable(&self, manifest: &HnswIndexManifest) -> bool {
        !self.levels.is_empty()
            && self.levels.iter().all(|lvl| {
                let cap = if lvl.level == 0 {
                    manifest.m_max0
                } else {
                    manifest.m_max
                };
                lvl.candidates.len() >= (cap / 2).max(1)
            })
    }
}

/// Speculative HNSW pre-warm result for a single logical key (both old-row
/// removal repair plans and new-row multi-level insertion pre-search/shrinks).
#[derive(Default)]
pub(crate) struct HnswPrewarmResult {
    pub(crate) inserts: BTreeMap<(usize, i32), HnswPresearch>,
    pub(crate) removals: BTreeMap<(usize, i32), Vec<PrecomputedLayerRemoval>>,
}

impl SessionTx<'_> {
    /// Speculatively pre-warm `vec_cache` (for `old_row`'s incident vertices,
    /// `new_row`'s multi-level `ef_construction` search, and any full-degree
    /// selected neighbours likely to shrink) and return pre-computed removal
    /// repair plans plus pre-searched candidates, tentative neighbour
    /// selections, and precomputed shrink heuristics for vectors whose random
    /// level does not become a new global entry point
    /// (`target_level >= bottom_level`). Purely read-only (`&self`).
    pub(crate) fn hnsw_presearch_and_warm(
        &self,
        old_row: Option<&Tuple>,
        new_row: Option<&Tuple>,
        manifest: &HnswIndexManifest,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
        vec_cache: &mut VectorCache<'_, '_>,
    ) -> Result<HnswPrewarmResult> {
        let mut res = HnswPrewarmResult::default();
        // 1. Warm incident neighbour vectors and pre-compute removal repair
        //    plans across all layers where `old_row` is indexed.
        if let Some(old) = old_row {
            let old_key = &old[..orig_table.index_key_len()];
            for &field in &manifest.vec_fields {
                let subs: Vec<i32> = match old.get(field) {
                    Some(DataValue::Vec(_)) => vec![-1],
                    Some(DataValue::List(l)) => l
                        .iter()
                        .enumerate()
                        .filter_map(|(i, v)| matches!(v, DataValue::Vec(_)).then_some(i as i32))
                        .collect(),
                    _ => vec![],
                };
                for sub in subs {
                    let compound = (old_key.to_vec(), field, sub);
                    let mut layer_plans = Vec::new();
                    for neg_layer in 0i64.. {
                        let layer = -neg_layer;
                        let mut self_key = Vec::with_capacity(old_key.len() * 2 + 5);
                        self_key.push(DataValue::from(layer));
                        for _ in 0..2 {
                            self_key.extend_from_slice(old_key);
                            self_key.push(DataValue::from(field as i64));
                            self_key.push(DataValue::from(sub as i64));
                        }
                        let self_key_bytes =
                            idx_table.encode_key_for_store(&self_key, Default::default())?;
                        if !self.idx_exists(idx_table, &self_key_bytes)? {
                            break;
                        }
                        if let Ok(plan) = self.hnsw_precompute_remove_layer(
                            &compound,
                            layer,
                            manifest,
                            orig_table,
                            idx_table,
                            vec_cache,
                        ) {
                            layer_plans.push(plan);
                        }
                    }
                    if !layer_plans.is_empty() {
                        res.removals.insert((field, sub), layer_plans);
                    }
                }
            }
        }
        // 2. Pre-search all levels `target_level..=0` for insertion
        //    (when `target_level >= bottom_level`, ~99.997% of ops) and
        //    pre-compute tentative neighbour selections + full-degree shrinks.
        let Some(new) = new_row else {
            return Ok(res);
        };
        let ep_res = idx_table
            .scan_bounded_prefix(
                self,
                &[],
                &[DataValue::from(i64::MIN)],
                &[DataValue::from(0)],
            )
            .next();
        let Some(ep_item) = ep_res else {
            return Ok(res);
        };
        let ep = ep_item?;
        let bottom_level = ep[0].get_int().unwrap();
        let ep_t_key = ep[1..orig_table.index_key_len() + 1].to_vec();
        let ep_idx = ep[orig_table.index_key_len() + 1].get_int().unwrap() as usize;
        let ep_subidx = ep[orig_table.index_key_len() + 2].get_int().unwrap() as i32;
        let ep_key = (ep_t_key, ep_idx, ep_subidx);
        vec_cache.ensure_key(&ep_key, orig_table, self)?;

        let mut extracted: Vec<(&Vector, usize, i32)> = Vec::new();
        for &idx in &manifest.vec_fields {
            match new.get(idx) {
                Some(DataValue::Vec(v)) => extracted.push((v, idx, -1)),
                Some(DataValue::List(l)) => {
                    for (sidx, v) in l.iter().enumerate() {
                        if let DataValue::Vec(v) = v {
                            extracted.push((v, idx, sidx as i32));
                        }
                    }
                }
                _ => {}
            }
        }
        let tuple_key = &new[..orig_table.index_key_len()];
        for (q, idx, subidx) in extracted {
            let target_level = manifest.get_random_level();
            if target_level < bottom_level {
                continue;
            }
            let inserted_key = (tuple_key.to_vec(), idx, subidx);
            let ep_distance = vec_cache.v_dist(q, &ep_key);
            let mut found_nn = PriorityQueue::new();
            found_nn.push(ep_key.clone(), OrderedFloat(ep_distance));
            for current_level in bottom_level..target_level {
                self.hnsw_search_level(
                    q,
                    1,
                    current_level,
                    orig_table,
                    idx_table,
                    &mut found_nn,
                    vec_cache,
                )?;
            }
            let mut levels = Vec::with_capacity((1 - target_level) as usize);
            let mut shrinks: FxHashMap<(i64, CompoundKey), PrecomputedShrink> =
                FxHashMap::default();
            for current_level in target_level..=0 {
                let m_max = if current_level == 0 {
                    manifest.m_max0
                } else {
                    manifest.m_max
                };
                self.hnsw_search_level(
                    q,
                    manifest.ef_construction,
                    current_level,
                    orig_table,
                    idx_table,
                    &mut found_nn,
                    vec_cache,
                )?;
                // Exclude any prior version of `tuple_key` (which will be removed
                // before insert inside `tt_commit_lock`) so `tentative_neighbours`
                // stays valid without needing recomputation.
                let mut level_candidates = found_nn.clone();
                let self_entries: Vec<CompoundKey> = level_candidates
                    .iter()
                    .filter(|(k, _)| k.0.as_slice() == tuple_key)
                    .map(|(k, _)| k.clone())
                    .collect();
                for k in self_entries {
                    level_candidates.remove(&k);
                }
                // When `extend_candidates` is enabled, `hnsw_select_neighbours_heuristic`
                // pulls 1-hop neighbours from storage outside `level_candidates` (which
                // can re-introduce `tuple_key` or concurrently mutated 2-hop keys not
                // tracked by `expected_existing`). Keep `level_candidates` and `vec_cache`
                // warm, but recompute neighbour selection and shrinks inside `tt_commit_lock`.
                let tentative_opt = if manifest.extend_candidates {
                    None
                } else {
                    let tentative = self.hnsw_select_neighbours_heuristic(
                        q,
                        &level_candidates,
                        m_max,
                        current_level,
                        manifest,
                        idx_table,
                        orig_table,
                        vec_cache,
                    )?;
                    // Pre-warm and pre-compute shrink heuristics for neighbours already
                    // at or above `m_max`. Temporarily insert `q` under `inserted_key`
                    // only during the local heuristic computation, then restore.
                    let mut full_neighbours: Vec<(CompoundKey, f64, Vec<(CompoundKey, f64)>)> =
                        Vec::new();
                    let mut shrink_warm: Vec<CompoundKey> = Vec::new();
                    let mut target_self_key =
                        Vec::with_capacity(orig_table.index_key_len() * 2 + 5);
                    for (neighbour, Reverse(OrderedFloat(dist_to_q))) in tentative.iter() {
                        if neighbour.0.as_slice() == tuple_key {
                            continue;
                        }
                        target_self_key.clear();
                        target_self_key.push(DataValue::from(current_level));
                        for _ in 0..2 {
                            target_self_key.extend_from_slice(&neighbour.0);
                            target_self_key.push(DataValue::from(neighbour.1 as i64));
                            target_self_key.push(DataValue::from(neighbour.2 as i64));
                        }
                        let self_key_bytes = idx_table
                            .encode_key_for_store(&target_self_key, Default::default())?;
                        if let Some(val_bytes) = self.idx_get(idx_table, &self_key_bytes)? {
                            if let Ok(val) = try_decode_val_only(&self_key_bytes, &val_bytes) {
                                if let Some(deg) = val.first().and_then(DataValue::get_float) {
                                    if (deg as usize) + 1 > m_max {
                                        let existing: Vec<(CompoundKey, f64)> = self
                                            .hnsw_get_neighbours(
                                                neighbour,
                                                current_level,
                                                idx_table,
                                                false,
                                            )?
                                            .filter(|(k, _)| k.0.as_slice() != tuple_key)
                                            .collect();
                                        if existing.len() + 1 > m_max {
                                            for (nb_key, _) in &existing {
                                                if !vec_cache.cache.contains_key(nb_key) {
                                                    shrink_warm.push(nb_key.clone());
                                                }
                                            }
                                            full_neighbours.push((
                                                neighbour.clone(),
                                                *dist_to_q,
                                                existing,
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if !shrink_warm.is_empty() {
                        shrink_warm.sort();
                        shrink_warm.dedup();
                        vec_cache.ensure_keys(&shrink_warm, orig_table, self)?;
                    }
                    if !full_neighbours.is_empty() {
                        let prev_self_vec =
                            vec_cache.insert(inserted_key.clone(), q.clone());
                        for (neighbour, dist_to_q, existing) in full_neighbours {
                            if vec_cache.ensure_key(&neighbour, orig_table, self).is_err() {
                                continue;
                            }
                            let nb_vec = vec_cache.get_key(&neighbour).clone();
                            let mut expected_existing: Vec<CompoundKey> =
                                existing.iter().map(|(k, _)| k.clone()).collect();
                            expected_existing.sort();
                            let mut shrink_cands =
                                PriorityQueue::with_capacity(existing.len() + 1);
                            for (k, d) in existing {
                                shrink_cands.push(k, OrderedFloat(d));
                            }
                            shrink_cands.push(inserted_key.clone(), OrderedFloat(dist_to_q));
                            let Ok(heuristic_pq) = self.hnsw_select_neighbours_heuristic(
                                &nb_vec,
                                &shrink_cands,
                                m_max,
                                current_level,
                                manifest,
                                idx_table,
                                orig_table,
                                vec_cache,
                            ) else {
                                continue;
                            };
                            let fast_drop = if expected_existing.len() == m_max {
                                self.hnsw_find_fast_drop(
                                    current_level,
                                    idx_table,
                                    &shrink_cands,
                                    &heuristic_pq,
                                    Some(&inserted_key),
                                )
                                .ok()
                                .flatten()
                            } else {
                                None
                            };
                            let heuristic: Vec<(CompoundKey, f64)> = heuristic_pq
                                .into_iter()
                                .map(|(k, Reverse(OrderedFloat(d)))| (k, d))
                                .collect();
                            shrinks.insert(
                                (current_level, neighbour),
                                PrecomputedShrink {
                                    expected_existing,
                                    heuristic,
                                    fast_drop,
                                },
                            );
                        }
                        match prev_self_vec {
                            Some(v) => {
                                vec_cache.insert(inserted_key.clone(), v);
                            }
                            None => {
                                vec_cache.cache.remove(&inserted_key);
                            }
                        }
                    }
                    Some(tentative)
                };
                levels.push(PresearchedLevel {
                    level: current_level,
                    candidates: level_candidates,
                    tentative_neighbours: tentative_opt,
                });
            }
            res.inserts.insert(
                (idx, subidx),
                HnswPresearch {
                    target_level,
                    levels,
                    shrinks,
                },
            );
        }
        Ok(res)
    }

    #[allow(clippy::too_many_arguments)]
    fn hnsw_put_vector(
        &mut self,
        tuple: &[DataValue],
        q: &Vector,
        idx: usize,
        subidx: i32,
        manifest: &HnswIndexManifest,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
        vec_cache: &mut VectorCache<'_, '_>,
        presearch: Option<HnswPresearch>,
        no_concurrent_mutations: bool,
        touched_by_remove: Option<&FxHashSet<CompoundKey>>,
    ) -> Result<()> {
        let tuple_key = &tuple[..orig_table.index_key_len()];
        let inserted_key = (tuple_key.to_vec(), idx, subidx);
        vec_cache.insert(inserted_key.clone(), q.clone());
        let hash = q.get_hash();
        let mut canary_tuple = vec![DataValue::from(0)];
        for _ in 0..2 {
            canary_tuple.extend_from_slice(tuple_key);
            canary_tuple.push(DataValue::from(idx as i64));
            canary_tuple.push(DataValue::from(subidx as i64));
        }
        if let Some(v) = idx_table.get(self, &canary_tuple)? {
            if let DataValue::Bytes(b) = &v[tuple_key.len() * 2 + 6] {
                if b == hash.as_ref() {
                    return Ok(());
                }
            }
            self.hnsw_remove_vec(
                tuple_key,
                idx,
                subidx,
                manifest,
                orig_table,
                idx_table,
                vec_cache,
                None,
                false,
                None,
            )?;
        }

        let ep_res = idx_table
            .scan_bounded_prefix(
                self,
                &[],
                &[DataValue::from(i64::MIN)],
                &[DataValue::from(0)],
            )
            .next();
        if let Some(ep) = ep_res {
            let ep = ep?;
            // bottom level since we are going up
            let bottom_level = ep[0].get_int().unwrap();
            let ep_t_key = ep[1..orig_table.index_key_len() + 1].to_vec();
            let ep_idx = ep[orig_table.index_key_len() + 1].get_int().unwrap() as usize;
            let ep_subidx = ep[orig_table.index_key_len() + 2].get_int().unwrap() as i32;
            let ep_key = (ep_t_key, ep_idx, ep_subidx);
            let presearch = presearch.filter(|pre| {
                pre.target_level >= bottom_level
                    && pre.levels.first().map(|l| l.level) == Some(pre.target_level)
                    && pre.levels.last().map(|l| l.level) == Some(0)
                    && pre.is_usable(manifest)
            });
            let (target_level, mut found_nn, mut pre_levels, pre_shrinks) = match presearch {
                Some(pre) => {
                    let level_map: BTreeMap<i64, PresearchedLevel> =
                        pre.levels.into_iter().map(|l| (l.level, l)).collect();
                    (
                        pre.target_level,
                        PriorityQueue::new(),
                        Some(level_map),
                        Some(pre.shrinks),
                    )
                }
                None => {
                    vec_cache.ensure_key(&ep_key, orig_table, self)?;
                    let ep_distance = vec_cache.v_dist(q, &ep_key);
                    let mut found_nn = PriorityQueue::new();
                    found_nn.push(ep_key, OrderedFloat(ep_distance));
                    let target_level = manifest.get_random_level();
                    if target_level < bottom_level {
                        // this becomes the entry point
                        self.hnsw_put_fresh_at_levels(
                            hash.as_ref(),
                            tuple_key,
                            idx,
                            subidx,
                            orig_table,
                            idx_table,
                            target_level,
                            bottom_level - 1,
                        )?;
                    }
                    for current_level in bottom_level..target_level {
                        self.hnsw_search_level(
                            q,
                            1,
                            current_level,
                            orig_table,
                            idx_table,
                            &mut found_nn,
                            vec_cache,
                        )?;
                    }
                    (target_level, found_nn, None, None)
                }
            };
            let mut self_tuple_key = Vec::with_capacity(orig_table.index_key_len() * 2 + 5);
            self_tuple_key.push(DataValue::from(0));
            for _ in 0..2 {
                self_tuple_key.extend_from_slice(tuple_key);
                self_tuple_key.push(DataValue::from(idx as i64));
                self_tuple_key.push(DataValue::from(subidx as i64));
            }
            let mut self_tuple_val = vec![
                DataValue::from(0.0),
                DataValue::Bytes(hash.as_ref().to_vec()),
                DataValue::from(false),
            ];
            for current_level in max(target_level, bottom_level)..=0 {
                let m_max = if current_level == 0 {
                    manifest.m_max0
                } else {
                    manifest.m_max
                };
                let neighbours = if let Some(pre_lvl) = pre_levels
                    .as_mut()
                    .and_then(|map| map.remove(&current_level))
                {
                    match pre_lvl.tentative_neighbours {
                        Some(tn) => tn,
                        None => self.hnsw_select_neighbours_heuristic(
                            q,
                            &pre_lvl.candidates,
                            m_max,
                            current_level,
                            manifest,
                            idx_table,
                            orig_table,
                            vec_cache,
                        )?,
                    }
                } else {
                    self.hnsw_search_level(
                        q,
                        manifest.ef_construction,
                        current_level,
                        orig_table,
                        idx_table,
                        &mut found_nn,
                        vec_cache,
                    )?;
                    self.hnsw_select_neighbours_heuristic(
                        q,
                        &found_nn,
                        m_max,
                        current_level,
                        manifest,
                        idx_table,
                        orig_table,
                        vec_cache,
                    )?
                };
                // add self-link
                self_tuple_key[0] = DataValue::from(current_level);
                self_tuple_val[0] = DataValue::from(neighbours.len() as f64);

                let self_tuple_key_bytes =
                    idx_table.encode_key_for_store(&self_tuple_key, Default::default())?;
                let self_tuple_val_bytes =
                    idx_table.encode_val_only_for_store(&self_tuple_val, Default::default())?;
                self.idx_put(idx_table, &self_tuple_key_bytes, &self_tuple_val_bytes)?;

                // add bidirectional links
                for (neighbour, Reverse(OrderedFloat(dist))) in neighbours.iter() {
                    let mut out_key = Vec::with_capacity(orig_table.index_key_len() * 2 + 5);
                    let out_val = vec![
                        DataValue::from(*dist),
                        DataValue::Null,
                        DataValue::from(false),
                    ];
                    out_key.push(DataValue::from(current_level));
                    out_key.extend_from_slice(tuple_key);
                    out_key.push(DataValue::from(idx as i64));
                    out_key.push(DataValue::from(subidx as i64));
                    out_key.extend_from_slice(&neighbour.0);
                    out_key.push(DataValue::from(neighbour.1 as i64));
                    out_key.push(DataValue::from(neighbour.2 as i64));
                    let out_key_bytes =
                        idx_table.encode_key_for_store(&out_key, Default::default())?;
                    let out_val_bytes =
                        idx_table.encode_val_only_for_store(&out_val, Default::default())?;
                    self.idx_put(idx_table, &out_key_bytes, &out_val_bytes)?;

                    let mut in_key = Vec::with_capacity(orig_table.index_key_len() * 2 + 5);
                    let in_val = vec![
                        DataValue::from(*dist),
                        DataValue::Null,
                        DataValue::from(false),
                    ];
                    in_key.push(DataValue::from(current_level));
                    in_key.extend_from_slice(&neighbour.0);
                    in_key.push(DataValue::from(neighbour.1 as i64));
                    in_key.push(DataValue::from(neighbour.2 as i64));
                    in_key.extend_from_slice(tuple_key);
                    in_key.push(DataValue::from(idx as i64));
                    in_key.push(DataValue::from(subidx as i64));

                    let in_key_bytes =
                        idx_table.encode_key_for_store(&in_key, Default::default())?;
                    let in_val_bytes =
                        idx_table.encode_val_only_for_store(&in_val, Default::default())?;
                    self.idx_put(idx_table, &in_key_bytes, &in_val_bytes)?;
                }

                // Pruning must see every selected link in this layer. Otherwise
                // paths through the new vertex's not-yet-written neighbours are
                // invisible and a feasible insertion can be rejected.
                let mut kept_links = 0usize;
                for (neighbour, _) in neighbours.iter() {
                    // shrink links if necessary
                    let mut target_self_key =
                        Vec::with_capacity(orig_table.index_key_len() * 2 + 5);
                    target_self_key.push(DataValue::from(current_level));
                    for _ in 0..2 {
                        target_self_key.extend_from_slice(&neighbour.0);
                        target_self_key.push(DataValue::from(neighbour.1 as i64));
                        target_self_key.push(DataValue::from(neighbour.2 as i64));
                    }
                    let target_self_key_bytes =
                        idx_table.encode_key_for_store(&target_self_key, Default::default())?;
                    let target_self_val_bytes = match self.idx_get(idx_table, &target_self_key_bytes)? {
                        Some(bytes) => bytes,
                        None => bail!("Indexed vector not found, this signifies a bug in the index implementation"),
                    };
                    let mut target_self_val =
                        try_decode_val_only(&target_self_key_bytes, &target_self_val_bytes)?;
                    let mut target_degree = target_self_val
                        .first()
                        .and_then(DataValue::get_float)
                        .ok_or_else(|| {
                        miette!("corrupt HNSW self-row: missing numeric degree")
                    })? as usize
                        + 1;
                    if target_degree > m_max {
                        // shrink links
                        let pre_shrink = pre_shrinks
                            .as_ref()
                            .and_then(|map| map.get(&(current_level, neighbour.clone())));
                        let skip_rescan = no_concurrent_mutations
                            && touched_by_remove.is_none_or(|s| !s.contains(neighbour));
                        let (degree, kept) = self.hnsw_shrink_neighbour(
                            neighbour,
                            &inserted_key,
                            m_max,
                            current_level,
                            manifest,
                            idx_table,
                            orig_table,
                            vec_cache,
                            pre_shrink,
                            skip_rescan,
                        )?;
                        target_degree = degree;
                        kept_links += usize::from(kept);
                    } else {
                        kept_links += 1;
                    }
                    // update degree
                    target_self_val[0] = DataValue::from(target_degree as f64);
                    let target_self_val_bytes_new = idx_table
                        .encode_val_only_for_store(&target_self_val, Default::default())?;
                    self.idx_put(
                        idx_table,
                        &target_self_key_bytes,
                        &target_self_val_bytes_new,
                    )?;
                }
                // Orphan guard (0.19.2): if every neighbour declined or pruned its
                // link back, nothing points at the new vertex on this layer and
                // no search can reach it. Give the nearest neighbour that can take
                // it the link back, keeping that neighbour's degree within m.
                if kept_links == 0 && !neighbours.is_empty() {
                    let mut by_distance: Vec<_> = neighbours
                        .iter()
                        .map(|(k, Reverse(OrderedFloat(d)))| (k.clone(), *d))
                        .collect();
                    by_distance.sort_by(|a, b| {
                        OrderedFloat(a.1)
                            .cmp(&OrderedFloat(b.1))
                            .then_with(|| a.0.cmp(&b.0))
                    });
                    self.hnsw_restore_in_link(
                        &inserted_key,
                        &by_distance,
                        current_level,
                        m_max,
                        idx_table,
                    )?;
                }
            }
        } else {
            // This is the first vector in the index.
            let level = manifest.get_random_level();
            self.hnsw_put_fresh_at_levels(
                hash.as_ref(),
                tuple_key,
                idx,
                subidx,
                orig_table,
                idx_table,
                level,
                0,
            )?;
        }
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    fn hnsw_shrink_neighbour(
        &mut self,
        target_key: &CompoundKey,
        inserted: &CompoundKey,
        m: usize,
        level: i64,
        manifest: &HnswIndexManifest,
        idx_table: &RelationHandle,
        orig_table: &RelationHandle,
        vec_cache: &mut VectorCache<'_, '_>,
        pre_shrink: Option<&PrecomputedShrink>,
        skip_rescan: bool,
    ) -> Result<(usize, bool)> {
        // Uncontended fast path: when zero concurrent commits touched this
        // relation since `pre_seq` and `target_key` was not touched by removal,
        // its active neighbour set is guaranteed identical to
        // `pre.expected_existing ∪ {inserted}`. Re-verify the 2-hop witness
        // edge live via a single point get (catching any earlier shrink in this
        // same insertion) and demote `dropped_v` directly without rescanning
        // `target_key`'s prefix.
        if skip_rescan {
            if let Some(pre) = pre_shrink {
                if pre.expected_existing.len() == m {
                    if let Some((dropped_v, witness_u)) = &pre.fast_drop {
                        if dropped_v != inserted
                            && self.repair_edge_is_live_fast(
                                idx_table, level, witness_u, dropped_v,
                            )?
                        {
                            let mut old_key =
                                Vec::with_capacity(orig_table.index_key_len() * 2 + 5);
                            old_key.push(DataValue::from(level));
                            old_key.extend_from_slice(&target_key.0);
                            old_key.push(DataValue::from(target_key.1 as i64));
                            old_key.push(DataValue::from(target_key.2 as i64));
                            old_key.extend_from_slice(&dropped_v.0);
                            old_key.push(DataValue::from(dropped_v.1 as i64));
                            old_key.push(DataValue::from(dropped_v.2 as i64));
                            let old_key_bytes =
                                idx_table.encode_key_for_store(&old_key, Default::default())?;
                            if let Some(old_existing_bytes) =
                                self.idx_get(idx_table, &old_key_bytes)?
                            {
                                let (old_dist, is_deleted) =
                                    match hnsw_edge_dist_and_deleted_fast(&old_existing_bytes) {
                                        Some(pair) => pair,
                                        None => {
                                            let decoded = try_decode_val_only(
                                                &old_key_bytes,
                                                &old_existing_bytes,
                                            )?;
                                            let d = decoded[0].get_float().ok_or_else(|| {
                                                miette!("corrupt HNSW edge row: missing distance")
                                            })?;
                                            let del = decoded
                                                .get(2)
                                                .and_then(DataValue::get_bool)
                                                .ok_or_else(|| {
                                                    miette!(
                                                        "corrupt HNSW edge row: missing deletion marker"
                                                    )
                                                })?;
                                            (d, del)
                                        }
                                    };
                                if is_deleted {
                                    self.idx_del(idx_table, &old_key_bytes)?;
                                } else {
                                    let old_val = vec![
                                        DataValue::from(old_dist),
                                        DataValue::Null,
                                        DataValue::from(true),
                                    ];
                                    let old_val_bytes = idx_table.encode_val_only_for_store(
                                        &old_val,
                                        Default::default(),
                                    )?;
                                    self.idx_put(idx_table, &old_key_bytes, &old_val_bytes)?;
                                }
                                return Ok((m, true));
                            }
                        }
                    }
                }
            }
        }
        let mut candidates = PriorityQueue::new();
        for (neighbour_key, neighbour_dist) in
            self.hnsw_get_neighbours(target_key, level, idx_table, false)?
        {
            candidates.push(neighbour_key, OrderedFloat(neighbour_dist));
        }
        // Check whether a precomputed shrink (computed outside `tt_commit_lock`)
        // matches `target_key`'s exact current active neighbour set.
        let matched_pre = pre_shrink.filter(|pre| {
            if candidates.get(inserted).is_none() || candidates.len() != pre.expected_existing.len() + 1 {
                return false;
            }
            let mut cur_existing: Vec<&CompoundKey> = candidates
                .iter()
                .map(|(k, _)| k)
                .filter(|k| *k != inserted)
                .collect();
            cur_existing.sort();
            cur_existing
                .iter()
                .zip(&pre.expected_existing)
                .all(|(a, b)| *a == b)
        });
        let fast_new_candidates = if let Some(pre) = matched_pre {
            if candidates.len() == m + 1 {
                if let Some((dropped_v, witness_u)) = &pre.fast_drop {
                    if dropped_v != inserted
                        && candidates.get(dropped_v).is_some()
                        && candidates.get(witness_u).is_some()
                        && self.repair_edge_is_live_fast(idx_table, level, witness_u, dropped_v)?
                    {
                        let mut kept = PriorityQueue::with_capacity(m);
                        for (k, d) in candidates.iter() {
                            if k != dropped_v {
                                kept.push(k.clone(), Reverse(*d));
                            }
                        }
                        Some(kept)
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        let new_candidates = if let Some(fast) = fast_new_candidates {
            fast
        } else {
            let heuristic = if let Some(pre) = matched_pre {
                let mut pq = PriorityQueue::with_capacity(pre.heuristic.len());
                for (k, d) in &pre.heuristic {
                    pq.push(k.clone(), Reverse(OrderedFloat(*d)));
                }
                pq
            } else {
                vec_cache.ensure_key(target_key, orig_table, self)?;
                let vec = vec_cache.get_key(target_key).clone();
                self.hnsw_select_neighbours_heuristic(
                    &vec,
                    &candidates,
                    m,
                    level,
                    manifest,
                    idx_table,
                    orig_table,
                    vec_cache,
                )?
            };
            match self.hnsw_preserve_pruned_paths(
                target_key,
                level,
                idx_table,
                &candidates,
                &heuristic,
                m,
                Some(inserted),
            ) {
                Ok(preserved) => preserved,
                // Path-preserving pruning is read-only and fails before any write,
                // so falling back to the heuristic selection (the pre-0.19 shrink)
                // starts from an untouched graph. Never fail the write for it.
                Err(err) => match repair::repair_limit_reason(&err) {
                    Some(reason) => {
                        repair::emit_repair_fallback(&orig_table.name, "insertion", level, reason);
                        let inserted_present = candidates.get(inserted).is_some();
                        if inserted_present && candidates.len() - 1 <= m {
                            // Keep every existing edge of the neighbour and decline only
                            // its new link to the inserted vertex: no existing row loses
                            // a path. The inserted vertex keeps its own outgoing links
                            // and any incoming links its other neighbours accepted.
                            let mut keep = PriorityQueue::new();
                            for (key, dist) in candidates.iter() {
                                if key != inserted {
                                    keep.push(key.clone(), Reverse(*dist));
                                }
                            }
                            keep
                        } else {
                            self.hnsw_protect_sole_in_edges(
                                target_key,
                                level,
                                idx_table,
                                &candidates,
                                heuristic,
                            )?
                        }
                    }
                    None => return Err(err),
                },
            }
        };
        let mut old_candidate_set = FxHashSet::default();
        for (old, _) in &candidates {
            old_candidate_set.insert(old.clone());
        }
        let mut new_candidate_set = FxHashSet::default();
        for (new, _) in &new_candidates {
            new_candidate_set.insert(new.clone());
        }
        let new_degree = new_candidates.len();
        let kept_inserted = new_candidate_set.contains(inserted);
        for (new, Reverse(OrderedFloat(new_dist))) in new_candidates {
            if !old_candidate_set.contains(&new) {
                let mut new_key = Vec::with_capacity(orig_table.index_key_len() * 2 + 5);
                let new_val = vec![
                    DataValue::from(new_dist),
                    DataValue::Null,
                    DataValue::from(false),
                ];
                new_key.push(DataValue::from(level));
                new_key.extend_from_slice(&target_key.0);
                new_key.push(DataValue::from(target_key.1 as i64));
                new_key.push(DataValue::from(target_key.2 as i64));
                new_key.extend_from_slice(&new.0);
                new_key.push(DataValue::from(new.1 as i64));
                new_key.push(DataValue::from(new.2 as i64));
                let new_key_bytes = idx_table.encode_key_for_store(&new_key, Default::default())?;
                let new_val_bytes =
                    idx_table.encode_val_only_for_store(&new_val, Default::default())?;
                self.idx_put(idx_table, &new_key_bytes, &new_val_bytes)?;
            }
        }
        for (old, OrderedFloat(old_dist)) in candidates {
            if !new_candidate_set.contains(&old) {
                let mut old_key = Vec::with_capacity(orig_table.index_key_len() * 2 + 5);
                old_key.push(DataValue::from(level));
                old_key.extend_from_slice(&target_key.0);
                old_key.push(DataValue::from(target_key.1 as i64));
                old_key.push(DataValue::from(target_key.2 as i64));
                old_key.extend_from_slice(&old.0);
                old_key.push(DataValue::from(old.1 as i64));
                old_key.push(DataValue::from(old.2 as i64));
                let old_key_bytes = idx_table.encode_key_for_store(&old_key, Default::default())?;
                let old_existing_val = match self.idx_get(idx_table, &old_key_bytes)? {
                    Some(bytes) => bytes,
                    None => {
                        bail!("Indexed vector not found, this signifies a bug in the index implementation")
                    }
                };
                let old_existing_val = try_decode_val_only(&old_key_bytes, &old_existing_val)?;
                let is_deleted = old_existing_val
                    .get(2)
                    .and_then(DataValue::get_bool)
                    .ok_or_else(|| miette!("corrupt HNSW edge row: missing deletion marker"))?;
                if is_deleted {
                    self.idx_del(idx_table, &old_key_bytes)?;
                } else {
                    let old_val = vec![
                        DataValue::from(old_dist),
                        DataValue::Null,
                        DataValue::from(true),
                    ];
                    let old_val_bytes =
                        idx_table.encode_val_only_for_store(&old_val, Default::default())?;
                    self.idx_put(idx_table, &old_key_bytes, &old_val_bytes)?;
                }
            }
        }

        Ok((new_degree, kept_inserted))
    }
    fn hnsw_select_neighbours_heuristic(
        &self,
        q: &Vector,
        found: &PriorityQueue<CompoundKey, OrderedFloat<f64>>,
        m: usize,
        level: i64,
        manifest: &HnswIndexManifest,
        idx_table: &RelationHandle,
        orig_table: &RelationHandle,
        vec_cache: &mut VectorCache<'_, '_>,
    ) -> Result<PriorityQueue<CompoundKey, Reverse<OrderedFloat<f64>>>> {
        let mut candidates = PriorityQueue::new();
        // Simple non-heuristic selection
        // let mut temp = found.clone();
        // while temp.len() > m {
        //     temp.pop();
        // }
        // for (item, dist) in temp.iter() {
        //     candidates.push(item.clone(), Reverse(*dist));
        // }
        // return Ok(candidates);
        // End of simple non-heuristic selection

        let mut ret: PriorityQueue<CompoundKey, Reverse<OrderedFloat<_>>> = PriorityQueue::new();
        let mut discarded: PriorityQueue<_, Reverse<OrderedFloat<_>>> = PriorityQueue::new();
        let found_keys: Vec<CompoundKey> = found.iter().map(|(item, _)| item.clone()).collect();
        vec_cache.ensure_keys(&found_keys, orig_table, self)?;
        for (item, dist) in found.iter() {
            // Add to candidates
            candidates.push(item.clone(), Reverse(*dist));
        }
        if manifest.extend_candidates {
            for (item, _) in found.iter() {
                // Extend by neighbours
                for (neighbour_key, _) in self.hnsw_get_neighbours(item, level, idx_table, false)? {
                    vec_cache.ensure_key(&neighbour_key, orig_table, self)?;
                    let dist = vec_cache.v_dist(q, &neighbour_key);
                    candidates.push(
                        (neighbour_key.0, neighbour_key.1, neighbour_key.2),
                        Reverse(OrderedFloat(dist)),
                    );
                }
            }
        }
        // Pre-resolve candidate vector references and cached squared norms once
        // outside the O(m^2) loop so pairwise comparisons do zero hash lookups
        // and zero redundant norm scans.
        let mut ordered: Vec<(CompoundKey, f64, &Vector, f64)> =
            Vec::with_capacity(candidates.len());
        while let Some((cand_key, Reverse(OrderedFloat(cand_dist_to_q)))) = candidates.pop() {
            let v = vec_cache.get_key(&cand_key);
            let norm_sq = vec_cache.vec_norm_sq(v);
            ordered.push((cand_key, cand_dist_to_q, v, norm_sq));
        }
        let mut selected: Vec<(&Vector, f64)> = Vec::with_capacity(m);
        for (cand_key, cand_dist_to_q, cand_v, cand_norm) in ordered {
            if ret.len() >= m {
                break;
            }
            let mut should_add = true;
            for &(existing_v, existing_norm) in &selected {
                let dist_to_existing =
                    vec_cache.dist_with_both_norms(existing_v, existing_norm, cand_v, cand_norm);
                if distance_is_closer(dist_to_existing, cand_dist_to_q) {
                    should_add = false;
                    break;
                }
            }
            if should_add {
                selected.push((cand_v, cand_norm));
                ret.push(cand_key, Reverse(OrderedFloat(cand_dist_to_q)));
            } else if manifest.keep_pruned_connections {
                discarded.push(cand_key, Reverse(OrderedFloat(cand_dist_to_q)));
            }
        }
        if manifest.keep_pruned_connections {
            while !discarded.is_empty() && ret.len() < m {
                let (nearest_triple, Reverse(OrderedFloat(nearest_dist))) =
                    discarded.pop().unwrap();
                ret.push(nearest_triple, Reverse(OrderedFloat(nearest_dist)));
            }
        }
        Ok(ret)
    }
    fn hnsw_search_level(
        &self,
        q: &Vector,
        ef: usize,
        cur_level: i64,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
        found_nn: &mut PriorityQueue<CompoundKey, OrderedFloat<f64>>,
        vec_cache: &mut VectorCache<'_, '_>,
    ) -> Result<()> {
        let q_norm_sq = match vec_cache.distance {
            HnswDistance::Cosine => match q {
                Vector::F32(a) => a
                    .as_slice()
                    .map(|s| s.iter().map(|&x| (x as f64) * (x as f64)).sum())
                    .unwrap_or_else(|| a.dot(a) as f64),
                Vector::F64(a) => a
                    .as_slice()
                    .map(|s| s.iter().map(|&x| x * x).sum())
                    .unwrap_or_else(|| a.dot(a)),
            },
            _ => 0.0,
        };
        let mut visited: FxHashSet<CompoundKey> = FxHashSet::default();
        // min queue
        let mut candidates: PriorityQueue<CompoundKey, Reverse<OrderedFloat<f64>>> =
            PriorityQueue::new();

        for item in found_nn.iter() {
            visited.insert(item.0.clone());
            candidates.push(item.0.clone(), Reverse(*item.1));
        }

        while let Some((candidate, Reverse(OrderedFloat(candidate_dist)))) = candidates.pop() {
            let (_, OrderedFloat(furthest_dist)) = found_nn.peek().unwrap();
            if distance_is_farther(candidate_dist, *furthest_dist) {
                break;
            }
            // Fetch all unvisited neighbours' vectors in one batched read
            // (mnestic fork): on the snapshot read path this is one RocksDB
            // MultiGet instead of a serial point get per neighbour.
            let unvisited =
                self.hnsw_get_unvisited_neighbours(&candidate, cur_level, idx_table, &visited)?;
            vec_cache.ensure_keys(&unvisited, orig_table, self)?;
            for neighbour_key in unvisited {
                if visited.contains(&neighbour_key) {
                    continue;
                }
                let neighbour_dist = vec_cache.v_dist_with_norm(q, q_norm_sq, &neighbour_key);
                let (_, OrderedFloat(candidate_furthest_dist)) = found_nn.peek().unwrap();
                if found_nn.len() < ef
                    || distance_is_closer(neighbour_dist, *candidate_furthest_dist)
                {
                    candidates.push(neighbour_key.clone(), Reverse(OrderedFloat(neighbour_dist)));
                    found_nn.push(neighbour_key.clone(), OrderedFloat(neighbour_dist));
                    if found_nn.len() > ef {
                        found_nn.pop();
                    }
                }
                visited.insert(neighbour_key);
            }
        }

        Ok(())
    }

    /// Fast neighbour-key scan for `hnsw_search_level` (E6): scans raw KV
    /// entries, validates the exact 42-byte MessagePack layout of standard HNSW
    /// edge values (`[Num(Float(_)), Null, Bool(is_deleted)]`) without
    /// allocating, and filters self-edges and already-visited neighbours inline.
    /// Any row that does not match the exact edge layout (self-rows carrying a
    /// vector hash, or any corrupt/truncated/colliding blob) falls back to
    /// `try_decode_val_only` so corruption errors remain unchanged.
    fn hnsw_get_unvisited_neighbours(
        &self,
        cand_key: &CompoundKey,
        level: i64,
        idx_handle: &RelationHandle,
        visited: &FxHashSet<CompoundKey>,
    ) -> Result<Vec<CompoundKey>> {
        let key_len = cand_key.0.len();
        let key_arity = 2 * key_len + 5;
        let mut start_tuple = Vec::with_capacity(key_len + 3);
        start_tuple.push(DataValue::from(level));
        start_tuple.extend_from_slice(&cand_key.0);
        start_tuple.push(DataValue::from(cand_key.1 as i64));
        start_tuple.push(DataValue::from(cand_key.2 as i64));

        let mut out = Vec::new();
        for res in idx_handle.scan_prefix_raw(self, &start_tuple) {
            let (kvec, vvec) = res?;
            let is_deleted = match hnsw_edge_is_deleted_fast(&vvec) {
                Some(b) => b,
                None => {
                    let vals = try_decode_val_only(&kvec, &vvec)?;
                    if vals.len() < 3 {
                        bail!(
                            "corrupt HNSW edge row: expected at least {} fields, got {}",
                            2 * key_len + 8,
                            key_arity + vals.len()
                        );
                    }
                    vals[2].get_bool().ok_or_else(|| {
                        miette!("corrupt HNSW edge row: deletion marker is not boolean")
                    })?
                }
            };
            if is_deleted {
                continue;
            }
            let tuple = crate::data::tuple::decode_tuple_from_key(&kvec, key_arity);
            if tuple.len() < key_arity {
                bail!(
                    "corrupt HNSW edge row: expected at least {} key fields, got {}",
                    key_arity,
                    tuple.len()
                );
            }
            let key_idx = tuple[2 * key_len + 3].get_int().ok_or_else(|| {
                miette!("corrupt HNSW edge row: vector index is not an integer")
            })? as usize;
            let key_subidx = tuple[2 * key_len + 4].get_int().ok_or_else(|| {
                miette!("corrupt HNSW edge row: vector sub-index is not an integer")
            })? as i32;
            let dst_slice = &tuple[key_len + 3..2 * key_len + 3];
            if dst_slice == cand_key.0.as_slice() && key_idx == cand_key.1 && key_subidx == cand_key.2
            {
                continue;
            }
            let neighbour_key = (dst_slice.to_vec(), key_idx, key_subidx);
            if !visited.contains(&neighbour_key) {
                out.push(neighbour_key);
            }
        }
        Ok(out)
    }
    fn hnsw_get_neighbours<'b>(
        &'b self,
        cand_key: &'b CompoundKey,
        level: i64,
        idx_handle: &RelationHandle,
        include_deleted: bool,
    ) -> Result<impl Iterator<Item = (CompoundKey, f64)> + 'b> {
        let key_len = cand_key.0.len();
        let key_arity = 2 * key_len + 5;
        let mut start_tuple = Vec::with_capacity(key_len + 3);
        start_tuple.push(DataValue::from(level));
        start_tuple.extend_from_slice(&cand_key.0);
        start_tuple.push(DataValue::from(cand_key.1 as i64));
        start_tuple.push(DataValue::from(cand_key.2 as i64));
        let mut neighbours = Vec::new();
        for res in idx_handle.scan_prefix_raw(self, &start_tuple) {
            let (kvec, vvec) = res?;
            let (distance, is_deleted) = match hnsw_edge_dist_and_deleted_fast(&vvec) {
                Some(pair) => pair,
                None => {
                    let vals = try_decode_val_only(&kvec, &vvec)?;
                    if vals.len() < 3 {
                        bail!(
                            "corrupt HNSW edge row: expected at least {} fields, got {}",
                            2 * key_len + 8,
                            key_arity + vals.len()
                        );
                    }
                    let distance = vals[0]
                        .get_float()
                        .ok_or_else(|| miette!("corrupt HNSW edge row: distance is not numeric"))?;
                    let is_deleted = vals[2].get_bool().ok_or_else(|| {
                        miette!("corrupt HNSW edge row: deletion marker is not boolean")
                    })?;
                    (distance, is_deleted)
                }
            };
            if !include_deleted && is_deleted {
                continue;
            }
            let tuple = crate::data::tuple::decode_tuple_from_key(&kvec, key_arity);
            if tuple.len() < key_arity {
                bail!(
                    "corrupt HNSW edge row: expected at least {} key fields, got {}",
                    key_arity,
                    tuple.len()
                );
            }
            let key_idx = tuple[2 * key_len + 3].get_int().ok_or_else(|| {
                miette!("corrupt HNSW edge row: vector index is not an integer")
            })? as usize;
            let key_subidx = tuple[2 * key_len + 4].get_int().ok_or_else(|| {
                miette!("corrupt HNSW edge row: vector sub-index is not an integer")
            })? as i32;
            let dst_slice = &tuple[key_len + 3..2 * key_len + 3];
            if dst_slice == cand_key.0.as_slice() && key_idx == cand_key.1 && key_subidx == cand_key.2
            {
                continue;
            }
            neighbours.push(((dst_slice.to_vec(), key_idx, key_subidx), distance));
        }
        Ok(neighbours.into_iter())
    }
    fn hnsw_put_fresh_at_levels(
        &mut self,
        hash: &[u8],
        tuple: &[DataValue],
        idx: usize,
        subidx: i32,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
        bottom_level: i64,
        top_level: i64,
    ) -> Result<()> {
        let mut target_key = vec![DataValue::Null];
        let mut canary_key = vec![DataValue::from(1)];
        for _ in 0..2 {
            for i in 0..orig_table.index_key_len() {
                target_key.push(tuple.get(i).unwrap().clone());
                canary_key.push(DataValue::Null);
            }
            target_key.push(DataValue::from(idx as i64));
            target_key.push(DataValue::from(subidx as i64));
            canary_key.push(DataValue::Null);
            canary_key.push(DataValue::Null);
        }
        let target_value = [
            DataValue::from(0.0),
            DataValue::Bytes(hash.to_vec()),
            DataValue::from(false),
        ];
        let target_key_bytes = idx_table.encode_key_for_store(&target_key, Default::default())?;

        // canary value is for conflict detection: prevent the scenario of disconnected graphs at all levels
        let canary_value = [
            DataValue::from(bottom_level),
            DataValue::Bytes(target_key_bytes),
            DataValue::from(false),
        ];
        let canary_key_bytes = idx_table.encode_key_for_store(&canary_key, Default::default())?;
        let canary_value_bytes =
            idx_table.encode_val_only_for_store(&canary_value, Default::default())?;
        self.idx_put(idx_table, &canary_key_bytes, &canary_value_bytes)?;

        for cur_level in bottom_level..=top_level {
            target_key[0] = DataValue::from(cur_level);
            let key = idx_table.encode_key_for_store(&target_key, Default::default())?;
            let val = idx_table.encode_val_only_for_store(&target_value, Default::default())?;
            self.idx_put(idx_table, &key, &val)?;
        }
        Ok(())
    }
    pub(crate) fn hnsw_put(
        &mut self,
        manifest: &HnswIndexManifest,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
        filter: Option<&Vec<Bytecode>>,
        stack: &mut Vec<DataValue>,
        tuple: &[DataValue],
    ) -> Result<bool> {
        // Steady-state path (single upsert): a fresh per-call cache, because a
        // batch can update a vector that is also some other row's neighbour, and
        // a cache shared across the batch could then serve a stale vector.
        let mut vec_cache = VectorCache::new(manifest.distance);
        self.hnsw_put_with_cache(
            manifest,
            orig_table,
            idx_table,
            filter,
            stack,
            tuple,
            &mut vec_cache,
        )
    }

    pub(crate) fn hnsw_put_with_cache(
        &mut self,
        manifest: &HnswIndexManifest,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
        filter: Option<&Vec<Bytecode>>,
        stack: &mut Vec<DataValue>,
        tuple: &[DataValue],
        vec_cache: &mut VectorCache<'_, '_>,
    ) -> Result<bool> {
        self.hnsw_put_with_cache_and_presearch(
            manifest, orig_table, idx_table, filter, stack, tuple, vec_cache, None, false, None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn hnsw_put_with_cache_and_presearch(
        &mut self,
        manifest: &HnswIndexManifest,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
        filter: Option<&Vec<Bytecode>>,
        stack: &mut Vec<DataValue>,
        tuple: &[DataValue],
        vec_cache: &mut VectorCache<'_, '_>,
        presearch: Option<&mut BTreeMap<(usize, i32), HnswPresearch>>,
        no_concurrent_mutations: bool,
        touched_by_remove: Option<&FxHashSet<CompoundKey>>,
    ) -> Result<bool> {
        self.hnsw_mutations_incomplete += 1;
        let result = self.hnsw_put_inner(
            manifest,
            orig_table,
            idx_table,
            filter,
            stack,
            tuple,
            vec_cache,
            presearch,
            no_concurrent_mutations,
            touched_by_remove,
        );
        if result.is_ok() {
            self.hnsw_mutations_incomplete -= 1;
        }
        result
    }

    pub(crate) fn hnsw_remove_with_cache(
        &mut self,
        manifest: &HnswIndexManifest,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
        tuple: &[DataValue],
        vec_cache: &mut VectorCache<'_, '_>,
    ) -> Result<()> {
        self.hnsw_remove_with_cache_and_pre(
            manifest, orig_table, idx_table, tuple, vec_cache, None, false, None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn hnsw_remove_with_cache_and_pre(
        &mut self,
        manifest: &HnswIndexManifest,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
        tuple: &[DataValue],
        vec_cache: &mut VectorCache<'_, '_>,
        pre_removals: Option<&BTreeMap<(usize, i32), Vec<PrecomputedLayerRemoval>>>,
        no_concurrent_mutations: bool,
        touched_neighbours: Option<&mut FxHashSet<CompoundKey>>,
    ) -> Result<()> {
        self.hnsw_mutations_incomplete += 1;
        let result = self.hnsw_remove_except(
            manifest,
            orig_table,
            idx_table,
            tuple,
            &FxHashSet::default(),
            vec_cache,
            pre_removals,
            no_concurrent_mutations,
            touched_neighbours,
        );
        if result.is_ok() {
            self.hnsw_mutations_incomplete -= 1;
        }
        result
    }

    /// Bulk-build an HNSW index over `tuples` (mnestic fork). The graph is
    /// constructed entirely in flat, integer-indexed memory (vector slab +
    /// per-node adjacency — see `hnsw_build.rs`), optionally with parallel
    /// insertion, then serialised into the index relation's tuple format in
    /// one pass. Produces the same row layout as the incremental path
    /// (`hnsw_put_vector`): per-level self rows carrying degree + vector hash,
    /// directed edge rows carrying distances, and the entry-point canary.
    pub(crate) fn hnsw_build_index(
        &mut self,
        manifest: &HnswIndexManifest,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
        filter: Option<&Vec<Bytecode>>,
        tuples: &[Tuple],
    ) -> Result<()> {
        use crate::runtime::hnsw_build::{build_threads, FlatHnswBuilder, NodeMeta};

        let key_len = orig_table.index_key_len();
        let mut builder = FlatHnswBuilder::new(manifest);
        let mut stack = vec![];
        for tuple in tuples {
            if let Some(code) = filter {
                if !eval_bytecode_pred(code, tuple, &mut stack, Default::default())? {
                    continue;
                }
            }
            for idx in &manifest.vec_fields {
                let val = tuple.get(*idx).unwrap();
                if let DataValue::Vec(v) = val {
                    builder.add_node(
                        NodeMeta {
                            key: tuple[..key_len].to_vec(),
                            field_idx: *idx,
                            sub_idx: -1,
                            hash: v.get_hash().as_ref().to_vec(),
                        },
                        v,
                    )?;
                } else if let DataValue::List(l) = val {
                    for (sidx, v) in l.iter().enumerate() {
                        if let DataValue::Vec(v) = v {
                            builder.add_node(
                                NodeMeta {
                                    key: tuple[..key_len].to_vec(),
                                    field_idx: *idx,
                                    sub_idx: sidx as i32,
                                    hash: v.get_hash().as_ref().to_vec(),
                                },
                                v,
                            )?;
                        }
                    }
                }
            }
        }
        if builder.is_empty() {
            return Ok(());
        }
        let build_start = std::time::Instant::now();
        let graph = builder.build(manifest, build_threads());
        let insert_elapsed = build_start.elapsed();

        // Serialise: per node × level, one self row + one row per out-edge.
        let mut key_buf: Vec<DataValue> = Vec::with_capacity(2 * key_len + 5);
        for (i, meta) in graph.metas.iter().enumerate() {
            for (d, list) in graph.towers[i].iter().enumerate() {
                let level = -(d as i64);
                key_buf.clear();
                key_buf.push(DataValue::from(level));
                for _ in 0..2 {
                    key_buf.extend_from_slice(&meta.key);
                    key_buf.push(DataValue::from(meta.field_idx as i64));
                    key_buf.push(DataValue::from(meta.sub_idx as i64));
                }
                let self_val = [
                    DataValue::from(list.len() as f64),
                    DataValue::Bytes(meta.hash.clone()),
                    DataValue::from(false),
                ];
                let k = idx_table.encode_key_for_store(&key_buf, Default::default())?;
                let v = idx_table.encode_val_only_for_store(&self_val, Default::default())?;
                self.idx_put(idx_table, &k, &v)?;

                for (nb, dist, ignore_link) in list
                    .iter()
                    .map(|&(nb, dist)| (nb, dist, false))
                    .chain(graph.dead[i][d].iter().map(|&(nb, dist)| (nb, dist, true)))
                {
                    let nb_meta = &graph.metas[nb as usize];
                    key_buf.clear();
                    key_buf.push(DataValue::from(level));
                    key_buf.extend_from_slice(&meta.key);
                    key_buf.push(DataValue::from(meta.field_idx as i64));
                    key_buf.push(DataValue::from(meta.sub_idx as i64));
                    key_buf.extend_from_slice(&nb_meta.key);
                    key_buf.push(DataValue::from(nb_meta.field_idx as i64));
                    key_buf.push(DataValue::from(nb_meta.sub_idx as i64));
                    let edge_val = [
                        DataValue::from(dist),
                        DataValue::Null,
                        DataValue::from(ignore_link),
                    ];
                    let k = idx_table.encode_key_for_store(&key_buf, Default::default())?;
                    let v = idx_table.encode_val_only_for_store(&edge_val, Default::default())?;
                    self.idx_put(idx_table, &k, &v)?;
                }
            }
        }

        // Entry-point canary, as written by `hnsw_put_fresh_at_levels`.
        if let Some((ep, ep_top)) = graph.entry {
            let ep_meta = &graph.metas[ep as usize];
            let mut target_key = vec![DataValue::Null];
            let mut canary_key = vec![DataValue::from(1)];
            for _ in 0..2 {
                for k in ep_meta.key.iter() {
                    target_key.push(k.clone());
                    canary_key.push(DataValue::Null);
                }
                target_key.push(DataValue::from(ep_meta.field_idx as i64));
                target_key.push(DataValue::from(ep_meta.sub_idx as i64));
                canary_key.push(DataValue::Null);
                canary_key.push(DataValue::Null);
            }
            let target_key_bytes =
                idx_table.encode_key_for_store(&target_key, Default::default())?;
            let canary_value = [
                DataValue::from(ep_top),
                DataValue::Bytes(target_key_bytes),
                DataValue::from(false),
            ];
            let canary_key_bytes =
                idx_table.encode_key_for_store(&canary_key, Default::default())?;
            let canary_value_bytes =
                idx_table.encode_val_only_for_store(&canary_value, Default::default())?;
            self.idx_put(idx_table, &canary_key_bytes, &canary_value_bytes)?;
        }
        if std::env::var_os("MNESTIC_BUILD_PROFILE").is_some() {
            eprintln!(
                "hnsw bulk build: {} nodes, graph {:?}, serialise {:?}",
                graph.metas.len(),
                insert_elapsed,
                build_start.elapsed() - insert_elapsed
            );
        }
        Ok(())
    }

    /// Serialise a built `FlatHnswGraph` into a flat sorted byte arena (3 heap
    /// buffers total, avoiding millions of tiny `BTreeMap` node allocations and
    /// allocator consolidation pauses during SST ingest).
    pub(crate) fn hnsw_serialise_graph_sorted(
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
        graph: crate::runtime::hnsw_build::FlatHnswGraph,
    ) -> Result<SortedKvArena> {
        let key_len = orig_table.index_key_len();
        let est_rows: usize = graph
            .towers
            .iter()
            .zip(&graph.dead)
            .map(|(t, d)| {
                t.iter()
                    .zip(d.iter())
                    .map(|(live, dead)| 1 + live.len() + dead.len())
                    .sum::<usize>()
            })
            .sum::<usize>()
            + 1;
        let mut arena = SortedKvArena {
            keys: Vec::with_capacity(est_rows * 48),
            vals: Vec::with_capacity(est_rows * 44),
            entries: Vec::with_capacity(est_rows),
        };
        let mut key_buf: Vec<DataValue> = Vec::with_capacity(2 * key_len + 5);
        for (i, meta) in graph.metas.iter().enumerate() {
            for (d, list) in graph.towers[i].iter().enumerate() {
                let level = -(d as i64);
                key_buf.clear();
                key_buf.push(DataValue::from(level));
                for _ in 0..2 {
                    key_buf.extend_from_slice(&meta.key);
                    key_buf.push(DataValue::from(meta.field_idx as i64));
                    key_buf.push(DataValue::from(meta.sub_idx as i64));
                }
                let self_val = [
                    DataValue::from(list.len() as f64),
                    DataValue::Bytes(meta.hash.clone()),
                    DataValue::from(false),
                ];
                let k = idx_table.encode_key_for_store(&key_buf, Default::default())?;
                let v = idx_table.encode_val_only_for_store(&self_val, Default::default())?;
                arena.push(&k, &v);

                for (nb, dist, ignore_link) in list
                    .iter()
                    .map(|&(nb, dist)| (nb, dist, false))
                    .chain(graph.dead[i][d].iter().map(|&(nb, dist)| (nb, dist, true)))
                {
                    let nb_meta = &graph.metas[nb as usize];
                    key_buf.clear();
                    key_buf.push(DataValue::from(level));
                    key_buf.extend_from_slice(&meta.key);
                    key_buf.push(DataValue::from(meta.field_idx as i64));
                    key_buf.push(DataValue::from(meta.sub_idx as i64));
                    key_buf.extend_from_slice(&nb_meta.key);
                    key_buf.push(DataValue::from(nb_meta.field_idx as i64));
                    key_buf.push(DataValue::from(nb_meta.sub_idx as i64));
                    let edge_val = [
                        DataValue::from(dist),
                        DataValue::Null,
                        DataValue::from(ignore_link),
                    ];
                    let k = idx_table.encode_key_for_store(&key_buf, Default::default())?;
                    let v = idx_table.encode_val_only_for_store(&edge_val, Default::default())?;
                    arena.push(&k, &v);
                }
            }
        }
        if let Some((ep, ep_top)) = graph.entry {
            let ep_meta = &graph.metas[ep as usize];
            let mut target_key = vec![DataValue::Null];
            let mut canary_key = vec![DataValue::from(1)];
            for _ in 0..2 {
                for k in ep_meta.key.iter() {
                    target_key.push(k.clone());
                    canary_key.push(DataValue::Null);
                }
                target_key.push(DataValue::from(ep_meta.field_idx as i64));
                target_key.push(DataValue::from(ep_meta.sub_idx as i64));
                canary_key.push(DataValue::Null);
                canary_key.push(DataValue::Null);
            }
            let target_key_bytes =
                idx_table.encode_key_for_store(&target_key, Default::default())?;
            let canary_value = [
                DataValue::from(ep_top),
                DataValue::Bytes(target_key_bytes),
                DataValue::from(false),
            ];
            let canary_key_bytes =
                idx_table.encode_key_for_store(&canary_key, Default::default())?;
            let canary_value_bytes =
                idx_table.encode_val_only_for_store(&canary_value, Default::default())?;
            arena.push(&canary_key_bytes, &canary_value_bytes);
        }
        arena.sort();
        Ok(arena)
    }

    #[allow(clippy::too_many_arguments)]
    fn hnsw_put_inner(
        &mut self,
        manifest: &HnswIndexManifest,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
        filter: Option<&Vec<Bytecode>>,
        stack: &mut Vec<DataValue>,
        tuple: &[DataValue],
        vec_cache: &mut VectorCache<'_, '_>,
        mut presearch: Option<&mut BTreeMap<(usize, i32), HnswPresearch>>,
        no_concurrent_mutations: bool,
        touched_by_remove: Option<&FxHashSet<CompoundKey>>,
    ) -> Result<bool> {
        if let Some(code) = filter {
            if !eval_bytecode_pred(code, tuple, stack, Default::default())? {
                self.hnsw_remove(manifest, orig_table, idx_table, tuple)?;
                return Ok(false);
            }
        }
        let mut extracted_vectors = vec![];
        for idx in &manifest.vec_fields {
            let val = tuple.get(*idx).unwrap();
            if let DataValue::Vec(v) = val {
                extracted_vectors.push((v, *idx, -1));
            } else if let DataValue::List(l) = val {
                for (sidx, v) in l.iter().enumerate() {
                    if let DataValue::Vec(v) = v {
                        extracted_vectors.push((v, *idx, sidx as i32));
                    }
                }
            }
        }
        let keep: FxHashSet<(usize, i32)> = extracted_vectors
            .iter()
            .map(|(_, idx, sub)| (*idx, *sub))
            .collect();
        self.hnsw_remove_except(
            manifest,
            orig_table,
            idx_table,
            tuple,
            &keep,
            vec_cache,
            None,
            false,
            None,
        )?;
        if extracted_vectors.is_empty() {
            return Ok(false);
        }
        let multi_vec = extracted_vectors.len() > 1;
        for (vec, idx, sub) in extracted_vectors {
            let pre = presearch.as_mut().and_then(|map| map.remove(&(idx, sub)));
            self.hnsw_put_vector(
                tuple,
                vec,
                idx,
                sub,
                manifest,
                orig_table,
                idx_table,
                vec_cache,
                pre,
                no_concurrent_mutations && !multi_vec,
                touched_by_remove,
            )?;
        }
        Ok(true)
    }
    pub(crate) fn hnsw_remove(
        &mut self,
        manifest: &HnswIndexManifest,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
        tuple: &[DataValue],
    ) -> Result<()> {
        let mut cache = VectorCache::new(manifest.distance);
        self.hnsw_mutations_incomplete += 1;
        let result = self.hnsw_remove_except(
            manifest,
            orig_table,
            idx_table,
            tuple,
            &FxHashSet::default(),
            &mut cache,
            None,
            false,
            None,
        );
        if result.is_ok() {
            self.hnsw_mutations_incomplete -= 1;
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn hnsw_remove_except(
        &mut self,
        manifest: &HnswIndexManifest,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
        tuple: &[DataValue],
        keep: &FxHashSet<(usize, i32)>,
        vec_cache: &mut VectorCache<'_, '_>,
        pre_removals: Option<&BTreeMap<(usize, i32), Vec<PrecomputedLayerRemoval>>>,
        no_concurrent_mutations: bool,
        mut touched_neighbours: Option<&mut FxHashSet<CompoundKey>>,
    ) -> Result<()> {
        let mut prefix = vec![DataValue::from(0)];
        prefix.extend_from_slice(&tuple[0..orig_table.index_key_len()]);
        let candidates: FxHashSet<_> = idx_table
            .scan_prefix(self, &prefix)
            .map(|t| {
                let t = t?;
                let key_len = orig_table.index_key_len();
                if t.len() < key_len + 3 {
                    bail!(
                        "corrupt HNSW self-row: expected at least {} fields, got {}",
                        key_len + 3,
                        t.len()
                    );
                }
                let idx = t[key_len + 1].get_int().ok_or_else(|| {
                    miette!("corrupt HNSW self-row: vector index is not an integer")
                })? as usize;
                let subidx = t[key_len + 2].get_int().ok_or_else(|| {
                    miette!("corrupt HNSW self-row: vector sub-index is not an integer")
                })? as i32;
                Ok((t[1..key_len + 1].to_vec(), idx, subidx))
            })
            .collect::<Result<_>>()?;
        let multi = candidates.len() > 1;
        for (tuple_key, idx, subidx) in candidates {
            if !keep.contains(&(idx, subidx)) {
                let pre = pre_removals.and_then(|m| m.get(&(idx, subidx)).map(Vec::as_slice));
                self.hnsw_remove_vec(
                    &tuple_key,
                    idx,
                    subidx,
                    manifest,
                    orig_table,
                    idx_table,
                    vec_cache,
                    pre,
                    no_concurrent_mutations && !multi,
                    touched_neighbours.as_deref_mut(),
                )?;
            }
        }
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    fn hnsw_remove_vec(
        &mut self,
        tuple_key: &[DataValue],
        idx: usize,
        subidx: i32,
        manifest: &HnswIndexManifest,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
        vec_cache: &mut VectorCache<'_, '_>,
        pre_removals: Option<&[PrecomputedLayerRemoval]>,
        no_concurrent_mutations: bool,
        mut touched_neighbours: Option<&mut FxHashSet<CompoundKey>>,
    ) -> Result<()> {
        let compound_key = (tuple_key.to_vec(), idx, subidx);
        // Go down the layers and remove all the links
        let mut encountered_singletons = false;
        for neg_layer in 0i64.. {
            let layer = -neg_layer;
            let mut self_key = vec![DataValue::from(layer)];
            for _ in 0..2 {
                self_key.extend_from_slice(tuple_key);
                self_key.push(DataValue::from(idx as i64));
                self_key.push(DataValue::from(subidx as i64));
            }
            let self_key_bytes = idx_table.encode_key_for_store(&self_key, Default::default())?;
            if self.idx_exists(idx_table, &self_key_bytes)? {
                let pre_layer = pre_removals.and_then(|s| s.iter().find(|p| p.layer == layer));
                // Every repair limit is checked before the repair writes, so on
                // a limit this layer is untouched and the unlink below proceeds
                // exactly as before 0.19. Never fail the write for it.
                if let Err(err) = self.hnsw_repair_before_remove_with_pre(
                    &compound_key,
                    layer,
                    manifest,
                    orig_table,
                    idx_table,
                    vec_cache,
                    pre_layer,
                    no_concurrent_mutations,
                ) {
                    match repair::repair_limit_reason(&err) {
                        Some(reason) => {
                            repair::emit_repair_fallback(
                                &orig_table.name,
                                "removal",
                                layer,
                                reason,
                            );
                            // Degree-preserving one-edge reconnection before the
                            // unlink, so in-neighbours are not cut off (0.19.1).
                            self.hnsw_reconnect_before_unlink(
                                &compound_key,
                                layer,
                                orig_table,
                                idx_table,
                                vec_cache,
                            )?;
                        }
                        None => return Err(err),
                    }
                }
                self.idx_del(idx_table, &self_key_bytes)?;
            } else {
                break;
            }

            let neigbours = self
                .hnsw_get_neighbours(&compound_key, layer, idx_table, true)?
                .collect_vec();
            encountered_singletons |= neigbours.is_empty();
            for (neighbour_key, _) in neigbours {
                if let Some(touched) = touched_neighbours.as_deref_mut() {
                    touched.insert(neighbour_key.clone());
                }
                let mut out_key = vec![DataValue::from(layer)];
                out_key.extend_from_slice(tuple_key);
                out_key.push(DataValue::from(idx as i64));
                out_key.push(DataValue::from(subidx as i64));
                out_key.extend_from_slice(&neighbour_key.0);
                out_key.push(DataValue::from(neighbour_key.1 as i64));
                out_key.push(DataValue::from(neighbour_key.2 as i64));
                let out_key_bytes = idx_table.encode_key_for_store(&out_key, Default::default())?;
                self.idx_del(idx_table, &out_key_bytes)?;
                let mut in_key = vec![DataValue::from(layer)];
                in_key.extend_from_slice(&neighbour_key.0);
                in_key.push(DataValue::from(neighbour_key.1 as i64));
                in_key.push(DataValue::from(neighbour_key.2 as i64));
                in_key.extend_from_slice(tuple_key);
                in_key.push(DataValue::from(idx as i64));
                in_key.push(DataValue::from(subidx as i64));
                let in_key_bytes = idx_table.encode_key_for_store(&in_key, Default::default())?;
                let incoming_bytes = self
                    .idx_get(idx_table, &in_key_bytes)?
                    .ok_or_else(|| miette!("corrupt HNSW index: missing reverse edge"))?;
                let incoming_active = match hnsw_edge_is_deleted_fast(&incoming_bytes) {
                    Some(del) => !del,
                    None => {
                        let incoming = try_decode_val_only(&in_key_bytes, &incoming_bytes)?;
                        !incoming[2]
                            .get_bool()
                            .ok_or_else(|| miette!("corrupt HNSW edge flag"))?
                    }
                };
                self.idx_del(idx_table, &in_key_bytes)?;
                if incoming_active {
                    let mut neighbour_self_key = vec![DataValue::from(layer)];
                    for _ in 0..2 {
                        neighbour_self_key.extend_from_slice(&neighbour_key.0);
                        neighbour_self_key.push(DataValue::from(neighbour_key.1 as i64));
                        neighbour_self_key.push(DataValue::from(neighbour_key.2 as i64));
                    }
                    let neighbour_self_key_bytes =
                        idx_table.encode_key_for_store(&neighbour_self_key, Default::default())?;
                    let neighbour_val_bytes = self
                        .idx_get(idx_table, &neighbour_self_key_bytes)?
                        .ok_or_else(|| {
                            miette!("corrupt HNSW index: neighbour self-row is missing")
                        })?;
                    let mut neighbour_val =
                        try_decode_val_only(&neighbour_self_key_bytes, &neighbour_val_bytes)?;
                    let degree = neighbour_val
                        .first()
                        .and_then(DataValue::get_float)
                        .ok_or_else(|| {
                            miette!("corrupt HNSW self-row: missing numeric degree")
                        })?;
                    neighbour_val[0] = DataValue::from(degree - 1.0);
                    let neighbour_val_bytes_new =
                        idx_table.encode_val_only_for_store(&neighbour_val, Default::default())?;
                    self.idx_put(
                        idx_table,
                        &neighbour_self_key_bytes,
                        &neighbour_val_bytes_new,
                    )?;
                }
            }
        }

        if encountered_singletons {
            // the entry point is removed, we need to do something
            let ep_res = idx_table
                .scan_bounded_prefix(
                    self,
                    &[],
                    &[DataValue::from(i64::MIN)],
                    &[DataValue::from(1)],
                )
                .next();
            let mut canary_key = vec![DataValue::from(1)];
            for _ in 0..2 {
                for _ in 0..orig_table.index_key_len() {
                    canary_key.push(DataValue::Null);
                }
                canary_key.push(DataValue::Null);
                canary_key.push(DataValue::Null);
            }
            let canary_key_bytes =
                idx_table.encode_key_for_store(&canary_key, Default::default())?;
            if let Some(ep) = ep_res {
                let ep = ep?;
                let target_key_bytes = idx_table.encode_key_for_store(&ep, Default::default())?;
                let bottom_level = ep[0].get_int().unwrap();
                // canary value is for conflict detection: prevent the scenario of disconnected graphs at all levels
                let canary_value = [
                    DataValue::from(bottom_level),
                    DataValue::Bytes(target_key_bytes),
                    DataValue::from(false),
                ];
                let canary_value_bytes =
                    idx_table.encode_val_only_for_store(&canary_value, Default::default())?;
                self.idx_put(idx_table, &canary_key_bytes, &canary_value_bytes)?;
            } else {
                // HA! we have removed the last item in the index
                self.idx_del(idx_table, &canary_key_bytes)?;
            }
        }

        Ok(())
    }
    pub(crate) fn hnsw_knn(
        &self,
        q: Vector,
        config: &HnswSearch,
        filter_bytecode: &Option<(Vec<Bytecode>, SourceSpan)>,
        stack: &mut Vec<DataValue>,
        poison: &crate::runtime::db::Poison,
    ) -> Result<Vec<Tuple>> {
        if q.len() != config.manifest.vec_dim {
            bail!("query vector dimension mismatch");
        }
        let q = match (q, config.manifest.dtype) {
            (v @ Vector::F32(_), VecElementType::F32) => v,
            (v @ Vector::F64(_), VecElementType::F64) => v,
            (Vector::F32(v), VecElementType::F64) => Vector::F64(v.mapv(|x| x as f64)),
            (Vector::F64(v), VecElementType::F32) => Vector::F32(v.mapv(|x| x as f32)),
        };

        let mut vec_cache = VectorCache::new(config.manifest.distance);

        if let Some(as_of_tt) = config.tx_valid_at {
            config
                .base_handle
                .check_tt_gc_floor(Some(as_of_tt), config.span)?;
            let rows = config.base_handle.skip_scan_all(self, as_of_tt);
            return Ok(self
                .hnsw_exact_scan_rows(
                    &q,
                    config,
                    filter_bytecode,
                    stack,
                    &vec_cache,
                    rows,
                    true,
                    poison,
                )?
                .expect("as-of exact scan either succeeds or returns an error"));
        }

        let ep_res = config
            .idx_handle
            .scan_bounded_prefix(
                self,
                &[],
                &[DataValue::from(i64::MIN)],
                &[DataValue::from(1)],
            )
            .next();
        if let Some(ep) = ep_res {
            let ep = ep?;
            let bottom_level = ep[0].get_int().unwrap();
            let ep_idx = match ep[config.base_handle.index_key_len() + 1].get_int() {
                Some(x) => x as usize,
                None => {
                    // this occurs if the index is empty
                    return Ok(vec![]);
                }
            };
            let ep_t_key = ep[1..config.base_handle.index_key_len() + 1].to_vec();
            let ep_subidx = ep[config.base_handle.index_key_len() + 2]
                .get_int()
                .unwrap() as i32;
            let ep_key = (ep_t_key, ep_idx, ep_subidx);
            vec_cache.ensure_key(&ep_key, &config.base_handle, self)?;
            let ep_distance = vec_cache.v_dist(&q, &ep_key);
            let mut found_nn = PriorityQueue::new();
            found_nn.push(ep_key, OrderedFloat(ep_distance));
            for current_level in bottom_level..0 {
                self.hnsw_search_level(
                    &q,
                    1,
                    current_level,
                    &config.base_handle,
                    &config.idx_handle,
                    &mut found_nn,
                    &mut vec_cache,
                )?;
            }
            self.hnsw_search_level(
                &q,
                config.ef,
                0,
                &config.base_handle,
                &config.idx_handle,
                &mut found_nn,
                &mut vec_cache,
            )?;
            if found_nn.is_empty() {
                return Ok(vec![]);
            }

            if config.filter.is_none() {
                while found_nn.len() > config.k {
                    found_nn.pop();
                }
            }

            let mut selected: Vec<(CompoundKey, f64)> = Vec::with_capacity(found_nn.len());
            while let Some((cand_key, OrderedFloat(distance))) = found_nn.pop() {
                if let Some(r) = config.radius {
                    if distance > r {
                        continue;
                    }
                }
                selected.push((cand_key, distance));
            }
            // Ascending distance order before batch fetching base rows.
            selected.reverse();

            let key_slices: Vec<&[DataValue]> =
                selected.iter().map(|(k, _)| k.0.as_slice()).collect();
            let base_rows = config.base_handle.get_current_batch(self, &key_slices)?;

            let mut ret = Vec::with_capacity(config.k.min(selected.len()));
            for ((cand_key, distance), base_row) in selected.into_iter().zip(base_rows) {
                let base_row = base_row.ok_or_else(|| miette!("corrupted index"))?;
                let cand_tuple = Self::hnsw_result_tuple(config, base_row, &cand_key, distance)?;

                if let Some((code, span)) = filter_bytecode {
                    if !eval_bytecode_pred(code, &cand_tuple, stack, *span)? {
                        continue;
                    }
                }

                ret.push(cand_tuple);
                if ret.len() >= config.k {
                    break;
                }
            }

            // Filtered completeness (0.19.2). The graph search visits about `ef`
            // vertices nearest the query regardless of the filter. If at least k
            // of them pass, any eligible row nearer than the k-th would have been
            // among them too, so the result is as good as unfiltered ANN. Fewer
            // than k passing means the window held too few eligible rows, so
            // score every eligible row exactly instead (bounded; see
            // `hnsw_filtered_exact`).
            if filter_bytecode.is_some() && ret.len() < config.k {
                if let Some(exact) = self.hnsw_filtered_exact(
                    &q,
                    config,
                    filter_bytecode,
                    stack,
                    &vec_cache,
                    poison,
                )? {
                    return Ok(exact);
                }
            }

            Ok(ret)
        } else {
            Ok(vec![])
        }
    }

    /// Exact top-k over every row that the index covers and the query filter
    /// accepts, in `(distance, key, field, subindex)` order. Returns `None`
    /// (keep the ANN result, warn `hnsw.filtered_incomplete`) past
    /// `FILTERED_EXACT_MAX_ROWS` base rows.
    fn hnsw_filtered_exact(
        &self,
        q: &Vector,
        config: &HnswSearch,
        filter_bytecode: &Option<(Vec<Bytecode>, SourceSpan)>,
        stack: &mut Vec<DataValue>,
        vec_cache: &VectorCache<'_, '_>,
        poison: &crate::runtime::db::Poison,
    ) -> Result<Option<Vec<Tuple>>> {
        let rows = config.base_handle.scan_current(self);
        self.hnsw_exact_scan_rows(
            q,
            config,
            filter_bytecode,
            stack,
            vec_cache,
            rows,
            false,
            poison,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn hnsw_exact_scan_rows<'b>(
        &self,
        q: &Vector,
        config: &HnswSearch,
        filter_bytecode: &Option<(Vec<Bytecode>, SourceSpan)>,
        stack: &mut Vec<DataValue>,
        vec_cache: &VectorCache<'_, '_>,
        rows: impl Iterator<Item = Result<Tuple>> + 'b,
        fail_on_limit: bool,
        poison: &crate::runtime::db::Poison,
    ) -> Result<Option<Vec<Tuple>>> {
        let index_filter = match &config.manifest.index_filter {
            Some(f_code) => {
                let parsed = CozoScriptParser::parse(Rule::expr, f_code)
                    .into_diagnostic()?
                    .next()
                    .unwrap();
                let mut code_expr = build_expr(parsed, &Default::default())?;
                code_expr.fill_binding_indices(&config.base_handle.raw_binding_map())?;
                Some(code_expr.compile()?)
            }
            None => None,
        };
        type Ranked = (OrderedFloat<f64>, Tuple, usize, i32);
        let mut best: std::collections::BinaryHeap<(Ranked, Tuple)> = Default::default();
        let n_keys = config.base_handle.index_key_len();
        let max_rows = if fail_on_limit {
            crate::runtime::txtime_index::as_of_scan_max_rows()
        } else {
            FILTERED_EXACT_MAX_ROWS
        };
        for (scanned, row) in rows.enumerate() {
            if scanned & 63 == 0 {
                poison.check()?;
            }
            if scanned >= max_rows {
                if fail_on_limit {
                    return Err(crate::runtime::txtime_index::as_of_scan_limit_error(
                        &config.base_handle.name,
                        max_rows,
                    ));
                }
                crate::runtime::diagnostics::emit(
                    "hnsw.filtered_incomplete",
                    format!(
                        "filtered vector search on '{}' found fewer than {} matches among its \
                         candidates, and the relation exceeds the {} rows an exact rescan may \
                         read; results may be incomplete",
                        config.base_handle.name, config.k, FILTERED_EXACT_MAX_ROWS
                    ),
                    "Create a partial index for the selective filter (`::hnsw create ... \
                     filter: <expr>`), or raise `ef`."
                        .to_string(),
                );
                return Ok(None);
            }
            let row = row?;
            if let Some(code) = &index_filter {
                if !eval_bytecode_pred(code, &row, stack, Default::default())? {
                    continue;
                }
            }
            let mut vectors = vec![];
            for &field in &config.manifest.vec_fields {
                match row.get(field) {
                    Some(DataValue::Vec(v)) => vectors.push((v.clone(), field, -1i32)),
                    Some(DataValue::List(l)) => {
                        for (sub, v) in l.iter().enumerate() {
                            if let DataValue::Vec(v) = v {
                                vectors.push((v.clone(), field, sub as i32));
                            }
                        }
                    }
                    _ => {}
                }
            }
            for (v, field, sub) in vectors {
                // The index only ever holds vectors of its own dtype and dimension.
                if v.len() != config.manifest.vec_dim
                    || std::mem::discriminant(&v) != std::mem::discriminant(q)
                {
                    continue;
                }
                let distance = vec_cache.dist(q, &v);
                if let Some(r) = config.radius {
                    if distance > r {
                        continue;
                    }
                }
                let rank: Ranked = (OrderedFloat(distance), row[..n_keys].to_vec(), field, sub);
                if best.len() >= config.k {
                    if let Some((worst, _)) = best.peek() {
                        if rank >= *worst {
                            continue;
                        }
                    }
                }
                let cand_key = (row[..n_keys].to_vec(), field, sub);
                let cand_tuple = Self::hnsw_result_tuple(config, row.clone(), &cand_key, distance)?;
                if let Some((code, span)) = filter_bytecode {
                    if !eval_bytecode_pred(code, &cand_tuple, stack, *span)? {
                        continue;
                    }
                }
                best.push((rank, cand_tuple));
                if best.len() > config.k {
                    best.pop();
                }
            }
        }
        Ok(Some(
            best.into_sorted_vec()
                .into_iter()
                .map(|(_, tuple)| tuple)
                .collect(),
        ))
    }

    /// A base row extended with the search's bound columns, in the order of
    /// `HnswSearch::all_bindings()`.
    fn hnsw_result_tuple(
        config: &HnswSearch,
        mut cand_tuple: Tuple,
        cand_key: &CompoundKey,
        distance: f64,
    ) -> Result<Tuple> {
        {
            {
                // make sure the order is the same as in all_bindings()!!!
                if config.bind_field.is_some() {
                    let field = if cand_key.1 < config.base_handle.metadata.keys.len() {
                        config.base_handle.metadata.keys[cand_key.1].name.clone()
                    } else {
                        config.base_handle.metadata.non_keys
                            [cand_key.1 - config.base_handle.metadata.keys.len()]
                        .name
                        .clone()
                    };
                    cand_tuple.push(DataValue::Str(field));
                }
                if config.bind_field_idx.is_some() {
                    cand_tuple.push(if cand_key.2 < 0 {
                        DataValue::Null
                    } else {
                        DataValue::from(cand_key.2 as i64)
                    });
                }
                if config.bind_distance.is_some() {
                    cand_tuple.push(DataValue::from(distance));
                }
                if config.bind_vector.is_some() {
                    let vec = if cand_key.2 < 0 {
                        cand_tuple[cand_key.1].clone()
                    } else {
                        match &cand_tuple[cand_key.1] {
                            DataValue::List(v) => v[cand_key.2 as usize].clone(),
                            v => bail!("corrupted index value {:?}", v),
                        }
                    };
                    cand_tuple.push(vec);
                }
            }
        }
        Ok(cand_tuple)
    }
}

/// Fast zero-allocation validator for standard 42-byte HNSW edge value blobs
/// (`[DataValue::Num(Num::Float(dist)), DataValue::Null, DataValue::Bool(is_deleted)]`
/// prefixed with the 8-byte `RelationId` header). Returns `Some(is_deleted)` only
/// when every structural byte matches; self-edges (which store a vector hash
/// instead of `Null`) and any corrupt/truncated/colliding blobs return `None` so
/// the caller falls back to `try_decode_val_only`.
/// Flat sorted key/value byte arena for streaming bulk HNSW graphs into
/// `Store::ingest_sorted` without allocating millions of `BTreeMap` nodes.
pub(crate) struct SortedKvArena {
    keys: Vec<u8>,
    vals: Vec<u8>,
    entries: Vec<(u64, u32, u64, u32)>,
}

impl SortedKvArena {
    pub(crate) fn new() -> Self {
        Self {
            keys: Vec::new(),
            vals: Vec::new(),
            entries: Vec::new(),
        }
    }

    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[inline]
    pub(crate) fn push(&mut self, k: &[u8], v: &[u8]) {
        let ks = self.keys.len() as u64;
        let kl = k.len() as u32;
        let vs = self.vals.len() as u64;
        let vl = v.len() as u32;
        self.keys.extend_from_slice(k);
        self.vals.extend_from_slice(v);
        self.entries.push((ks, kl, vs, vl));
    }

    pub(crate) fn sort(&mut self) {
        let keys = &self.keys;
        // Stable sort preserves insertion order for duplicate keys (e.g., if
        // duplicate columns appear in `fields`), and deduplicating in reverse
        // keeps the last written value (matching BTreeMap last-write-wins so
        // `SstFileWriter::put` always receives strictly ascending keys).
        self.entries.sort_by(|a, b| {
            let ka = &keys[a.0 as usize..a.0 as usize + a.1 as usize];
            let kb = &keys[b.0 as usize..b.0 as usize + b.1 as usize];
            ka.cmp(kb)
        });
        if self.entries.len() > 1 {
            let mut deduped: Vec<(u64, u32, u64, u32)> = Vec::with_capacity(self.entries.len());
            for entry in self.entries.iter().copied() {
                if let Some(prev) = deduped.last_mut() {
                    let kp = &keys[prev.0 as usize..prev.0 as usize + prev.1 as usize];
                    let ke = &keys[entry.0 as usize..entry.0 as usize + entry.1 as usize];
                    if kp == ke {
                        *prev = entry;
                        continue;
                    }
                }
                deduped.push(entry);
            }
            self.entries = deduped;
        }
    }

    pub(crate) fn iter_owned(
        &self,
    ) -> impl Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + '_ {
        self.entries.iter().map(|&(ks, kl, vs, vl)| {
            let k = self.keys[ks as usize..ks as usize + kl as usize].to_vec();
            let v = self.vals[vs as usize..vs as usize + vl as usize].to_vec();
            Ok((k, v))
        })
    }
}

#[inline]
pub(super) fn hnsw_edge_dist_and_deleted_fast(vvec: &[u8]) -> Option<(f64, bool)> {
    // Bytes 0..8: 8-byte RelationId header.
    // Bytes 8..22: fixarray(3) + fixmap(1) "Num" + fixmap(1) "Float" + f64(0xcb).
    const EDGE_PREFIX: [u8; 14] = [
        0x93, 0x81, 0xa3, b'N', b'u', b'm', 0x81, 0xa5, b'F', b'l', b'o', b'a', b't', 0xcb,
    ];
    // Bytes 22..30: 8-byte big-endian IEEE 754 f64 distance.
    // Bytes 30..35: fixstr(4) "Null" (DataValue::Null).
    const EDGE_NULL: [u8; 5] = [0xa4, b'N', b'u', b'l', b'l'];
    // Bytes 35..42: fixmap(1) "Bool" + false(0xc2)/true(0xc3).
    const BOOL_FALSE_TAIL: [u8; 7] = [0x81, 0xa4, b'B', b'o', b'o', b'l', 0xc2];
    const BOOL_TRUE_TAIL: [u8; 7] = [0x81, 0xa4, b'B', b'o', b'o', b'l', 0xc3];

    if vvec.len() != 42 || vvec[8..22] != EDGE_PREFIX || vvec[30..35] != EDGE_NULL {
        return None;
    }
    let is_deleted = if vvec[35..42] == BOOL_FALSE_TAIL {
        false
    } else if vvec[35..42] == BOOL_TRUE_TAIL {
        true
    } else {
        return None;
    };
    let dist = f64::from_be_bytes(vvec[22..30].try_into().ok()?);
    Some((dist, is_deleted))
}

#[inline]
pub(super) fn hnsw_edge_is_deleted_fast(vvec: &[u8]) -> Option<bool> {
    hnsw_edge_dist_and_deleted_fast(vvec).map(|(_, del)| del)
}

/// Base rows an exact filtered rescan may read before giving up (0.19.2).
const FILTERED_EXACT_MAX_ROWS: usize = 200_000;

#[cfg(test)]
mod tests {
    use super::{
        hnsw_edge_dist_and_deleted_fast, hnsw_edge_is_deleted_fast, HnswDistance, VectorCache,
    };
    use crate::data::value::{DataValue, Vector};
    use ndarray::Array1;
    use rand::Rng;
    use std::collections::BTreeMap;

    #[test]
    fn test_random_level() {
        let m = 20;
        let mult = 1. / (m as f64).ln();
        let mut rng = rand::thread_rng();
        let mut collected = BTreeMap::new();
        for _ in 0..10000 {
            let uniform_num: f64 = rng.gen_range(0.0..1.0);
            let r = -uniform_num.ln() * mult;
            // the level is the largest integer smaller than r
            let level = -(r.floor() as i64);
            collected.entry(level).and_modify(|x| *x += 1).or_insert(1);
        }
        println!("{:?}", collected);
    }

    #[test]
    fn hnsw_edge_fast_path_matches_wire_format_and_rejects_corrupt_blobs() {
        let encode = |vals: &[DataValue]| -> Vec<u8> {
            let mut out = vec![0u8; 8];
            rmp_serde::encode::write(&mut out, vals).unwrap();
            out
        };
        let live = encode(&[DataValue::from(0.25f64), DataValue::Null, DataValue::from(false)]);
        assert_eq!(live.len(), 42);
        assert_eq!(hnsw_edge_is_deleted_fast(&live), Some(false));
        assert_eq!(hnsw_edge_dist_and_deleted_fast(&live), Some((0.25, false)));

        let deleted = encode(&[DataValue::from(1.75f64), DataValue::Null, DataValue::from(true)]);
        assert_eq!(deleted.len(), 42);
        assert_eq!(hnsw_edge_is_deleted_fast(&deleted), Some(true));
        assert_eq!(hnsw_edge_dist_and_deleted_fast(&deleted), Some((1.75, true)));

        // Self-edge carries Bytes(hash) instead of Null -> falls back.
        let self_edge = encode(&[
            DataValue::from(16.0f64),
            DataValue::Bytes(vec![1, 2, 3, 4]),
            DataValue::from(false),
        ]);
        assert_eq!(hnsw_edge_is_deleted_fast(&self_edge), None);

        // Truncated 7-byte blob ending in BOOL_FALSE_TAIL -> must fall back.
        assert_eq!(
            hnsw_edge_is_deleted_fast(&[0x81, 0xa4, b'B', b'o', b'o', b'l', 0xc2]),
            None
        );

        // Wrong arity ending in Bool(false) -> must fall back.
        let short_arity = encode(&[DataValue::from(false)]);
        assert_eq!(hnsw_edge_is_deleted_fast(&short_arity), None);

        // Bit-flipped distance header -> must fall back.
        let mut flipped = live.clone();
        flipped[8] ^= 0x01;
        assert_eq!(hnsw_edge_is_deleted_fast(&flipped), None);
    }

    #[test]
    fn cosine_f32_self_distance_is_zero_and_symmetric() {
        let vc = VectorCache::new(HnswDistance::Cosine);
        let v1 = Vector::F32(Array1::from_vec(
            (0..384).map(|i| ((i as f32) * 0.137).sin() + 0.01).collect(),
        ));
        let v2 = Vector::F32(Array1::from_vec(
            (0..384).map(|i| ((i as f32) * 0.293).cos() - 0.02).collect(),
        ));
        let self_d = vc.dist(&v1, &v1);
        assert_eq!(self_d, 0.0, "cosine self-distance must be exact 0.0");
        let d12 = vc.dist(&v1, &v2);
        let d21 = vc.dist(&v2, &v1);
        assert_eq!(d12, d21, "cosine distance must be symmetric");
        assert!((0.0..=2.0).contains(&d12));
    }
}
