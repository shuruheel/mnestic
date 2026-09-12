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
use crate::data::value::{ValidityTs, Vector};
use crate::parse::sys::HnswDistance;
use crate::runtime::relation::{try_decode_val_only, RelationHandle};
use crate::runtime::transact::SessionTx;
use crate::storage::StoreTx;
use crate::{DataValue, SourceSpan};
use itertools::Itertools;
use miette::{bail, miette, Result};
use ordered_float::OrderedFloat;
use priority_queue::PriorityQueue;
use rand::Rng;
use rustc_hash::{FxHashMap, FxHashSet};
use smartstring::{LazyCompact, SmartString};
use std::cmp::{max, Reverse};

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

type CompoundKey = (Tuple, usize, i32);

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

struct VectorCache {
    cache: FxHashMap<CompoundKey, Vector>,
    /// Base rows for records already fetched, keyed by the record key so the several compound
    /// keys of one row share an entry. Populated only when a search predicate needs the row.
    rows: FxHashMap<Vec<DataValue>, Tuple>,
    /// Record keys this transaction cannot read.
    missing: FxHashSet<Vec<DataValue>>,
    /// Whether an unreadable record is fatal. Index maintenance says yes: it is writing the
    /// graph against rows that must be there, and a miss means the two have diverged. Search
    /// says no: the graph and the records it names are read independently, so a node the
    /// reader cannot resolve is a visibility outcome rather than damage, and the traversal
    /// steps over it.
    lenient: bool,
    /// Whether to retain fetched rows in `rows`.
    keep_rows: bool,
    distance: HnswDistance,
}

impl VectorCache {
    fn insert(&mut self, k: CompoundKey, v: Vector) {
        self.cache.insert(k, v);
    }
    fn dist(&self, v1: &Vector, v2: &Vector) -> f64 {
        match self.distance {
            HnswDistance::L2 => match (v1, v2) {
                (Vector::F32(a), Vector::F32(b)) => {
                    let diff = a - b;
                    diff.dot(&diff) as f64
                }
                (Vector::F64(a), Vector::F64(b)) => {
                    let diff = a - b;
                    diff.dot(&diff)
                }
                _ => panic!("Cannot compute L2 distance between {:?} and {:?}", v1, v2),
            },
            HnswDistance::Cosine => match (v1, v2) {
                (Vector::F32(a), Vector::F32(b)) => {
                    let a_norm = a.dot(a) as f64;
                    let b_norm = b.dot(b) as f64;
                    let dot = a.dot(b) as f64;
                    cosine_distance(a_norm, b_norm, dot)
                }
                (Vector::F64(a), Vector::F64(b)) => {
                    let a_norm = a.dot(a);
                    let b_norm = b.dot(b);
                    let dot = a.dot(b);
                    cosine_distance(a_norm, b_norm, dot)
                }
                _ => panic!(
                    "Cannot compute cosine distance between {:?} and {:?}",
                    v1, v2
                ),
            },
            HnswDistance::InnerProduct => match (v1, v2) {
                (Vector::F32(a), Vector::F32(b)) => {
                    let dot = a.dot(b);
                    1. - dot as f64
                }
                (Vector::F64(a), Vector::F64(b)) => {
                    let dot = a.dot(b);
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
    fn k_dist(&self, k1: &CompoundKey, k2: &CompoundKey) -> f64 {
        let v1 = self.cache.get(k1).unwrap();
        let v2 = self.cache.get(k2).unwrap();
        self.dist(v1, v2)
    }
    fn get_key(&self, key: &CompoundKey) -> &Vector {
        self.cache.get(key).unwrap()
    }
    /// Whether `key` names a record this transaction could not read. Always false unless the
    /// cache is lenient, because otherwise the miss would have been an error.
    #[inline]
    fn is_missing(&self, key: &CompoundKey) -> bool {
        !self.missing.is_empty() && self.missing.contains(&key.0)
    }

    /// Record a fetched row: extract its vector, and retain the row itself if asked.
    fn accept(&mut self, key: &CompoundKey, tuple: Tuple) -> Result<()> {
        self.insert_from_tuple(key, &tuple)?;
        if self.keep_rows {
            self.rows.insert(key.0.clone(), tuple);
        }
        Ok(())
    }

    /// Record a row that was not there.
    fn reject(&mut self, key: &CompoundKey) -> Result<()> {
        if !self.lenient {
            bail!("Cannot find compound key for HNSW: {:?}", key);
        }
        self.missing.insert(key.0.clone());
        Ok(())
    }

    /// Whether `key` still needs fetching: not already cached, and not already known absent.
    fn wanted(&self, key: &CompoundKey) -> bool {
        !self.cache.contains_key(key) && !self.is_missing(key)
    }

    fn ensure_key(
        &mut self,
        key: &CompoundKey,
        handle: &RelationHandle,
        tx: &SessionTx<'_>,
    ) -> Result<()> {
        if !self.wanted(key) {
            return Ok(());
        }
        // A sibling compound key of the same record may already have brought the row in.
        if self.keep_rows {
            if let Some(tuple) = self.rows.get(&key.0) {
                let tuple = tuple.clone();
                return self.insert_from_tuple(key, &tuple);
            }
        }
        match handle.get(tx, &key.0)? {
            Some(tuple) => self.accept(key, tuple),
            None => self.reject(key),
        }
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
        let wanted: Vec<&CompoundKey> = keys.iter().filter(|k| self.wanted(k)).collect();
        if wanted.is_empty() {
            return Ok(());
        }
        if wanted.len() == 1 {
            return self.ensure_key(wanted[0], handle, tx);
        }
        let key_slices: Vec<&[DataValue]> = wanted.iter().map(|k| k.0.as_slice()).collect();
        let tuples = handle.get_batch(tx, &key_slices)?;
        for (&key, tuple) in wanted.iter().zip(tuples) {
            match tuple {
                Some(tuple) => self.accept(key, tuple)?,
                None => self.reject(key)?,
            }
        }
        Ok(())
    }

    fn insert_from_tuple(&mut self, key: &CompoundKey, tuple: &[DataValue]) -> Result<()> {
        let mut field = &tuple[key.1];
        if key.2 >= 0 {
            match field {
                DataValue::List(l) => {
                    field = &l[key.2 as usize];
                }
                _ => bail!("Cannot interpret {} as list", field),
            }
        }
        match field {
            DataValue::Vec(v) => {
                self.cache.insert(key.clone(), v.clone());
            }
            _ => bail!("Cannot interpret {} as vector", field),
        }
        Ok(())
    }
}

/// Whether a node at `distance` is worth expanding: the result set is not yet full, or the
/// node beats its furthest member.
///
/// An unfilled set admits without comparing, which matters for a distance that is not a
/// number: `OrderedFloat` sorts NaN above every real distance, so comparing would drop such a
/// node from the traversal entirely and cut the graph around it.
fn may_expand(
    found_nn: &PriorityQueue<CompoundKey, OrderedFloat<f64>>,
    ef: usize,
    distance: f64,
) -> bool {
    match found_nn.peek() {
        Some((_, OrderedFloat(furthest))) if found_nn.len() >= ef => {
            distance_is_closer(distance, *furthest)
        }
        _ => true,
    }
}

/// Whether the walk is done: the result set is full and the nearest candidate left is further
/// away than everything in it. An unfilled set never stops the walk, which is what keeps a
/// selective search going instead of returning short.
fn is_exhausted(
    found_nn: &PriorityQueue<CompoundKey, OrderedFloat<f64>>,
    ef: usize,
    distance: f64,
) -> bool {
    match found_nn.peek() {
        Some((_, OrderedFloat(furthest))) if found_nn.len() >= ef => {
            distance_is_farther(distance, *furthest)
        }
        _ => false,
    }
}

/// The conditions a node must meet to be returned by a search, applied during the traversal.
///
/// These were once a pass over the finished result set, which silently capped a search at
/// however many of its first `ef` nodes happened to qualify. Asking here instead means `k`
/// results come back whenever `k` qualifying nodes are reachable at all.
///
/// `radius` is not among them, and stays a cut over the finished set. It bounds distance, and
/// the result set is ordered by distance, so the nearest `k` are the nearest `k` within any
/// radius that holds `k` of them: there is no shortfall to fix. Asking it during the walk would
/// also strand a search whose entry point happens to lie outside the radius.
struct HnswAdmit<'a> {
    config: &'a HnswSearch,
    filter: &'a Option<(Vec<Bytecode>, SourceSpan)>,
    /// Scratch for filter evaluation, both reused across candidates.
    stack: Vec<DataValue>,
    tuple: Tuple,
    /// Per record, the version of it live at the query point, or `None` where it has none.
    ///
    /// Every version of a record is its own node, so a walk reaches the same record through
    /// several of them and would otherwise ask the store the same question once per version.
    live: FxHashMap<Tuple, Option<DataValue>>,
}

impl<'a> HnswAdmit<'a> {
    fn new(config: &'a HnswSearch, filter: &'a Option<(Vec<Bytecode>, SourceSpan)>) -> Self {
        HnswAdmit {
            config,
            filter,
            stack: vec![],
            tuple: vec![],
            live: FxHashMap::default(),
        }
    }

    /// Whether anything is actually asked of a candidate. When nothing is, the search reverts
    /// to the plain nearest-neighbour walk.
    fn is_trivial(&self) -> bool {
        self.config.validity.is_none() && self.filter.is_none()
    }

    /// Whether `key` names the version of its record that is live at `valid_at`: an assertion,
    /// and the newest one no later than that point.
    fn is_live(
        &mut self,
        tx: &SessionTx<'_>,
        key: &CompoundKey,
        valid_at: ValidityTs,
    ) -> Result<bool> {
        let n_keys = self.config.base_handle.metadata.keys.len();
        let DataValue::Validity(vld) = &key.0[n_keys - 1] else {
            bail!(
                "relation '{}' has no validity column",
                self.config.base_handle.name
            );
        };
        // Both of these are decidable from the key alone, so they never reach the store: a
        // retraction is never live, and neither is a version later than the query point.
        // Validity sorts newest first, so a later one compares less.
        if !vld.is_assert.0 || vld.timestamp < valid_at {
            return Ok(false);
        }
        let prefix = &key.0[..n_keys - 1];
        if let Some(live) = self.live.get(prefix) {
            return Ok(live.as_ref() == Some(&key.0[n_keys - 1]));
        }
        let live = tx.hnsw_live_version(self.config, prefix, valid_at)?;
        let matched = live.as_ref() == Some(&key.0[n_keys - 1]);
        self.live.insert(prefix.to_vec(), live);
        Ok(matched)
    }

    fn admits(
        &mut self,
        tx: &SessionTx<'_>,
        key: &CompoundKey,
        distance: f64,
        vec_cache: &VectorCache,
    ) -> Result<bool> {
        if let Some(valid_at) = self.config.validity {
            if !self.is_live(tx, key, valid_at)? {
                return Ok(false);
            }
        }
        let Some((code, span)) = self.filter else {
            return Ok(true);
        };
        let Some(row) = vec_cache.rows.get(&key.0) else {
            return Ok(false);
        };
        // Refilled rather than rebuilt, so a wide selective walk does not allocate a tuple
        // per candidate it tests.
        self.tuple.clear();
        self.tuple.extend_from_slice(row);
        push_search_bindings(&mut self.tuple, key, self.config, distance)?;
        eval_bytecode_pred(code, &self.tuple, &mut self.stack, *span)
    }
}

/// Append the optional bindings a query asked for to a base row, turning it into the tuple a
/// search returns for that node. The order has to match `HnswSearch::all_bindings`.
fn push_search_bindings(
    tuple: &mut Tuple,
    key: &CompoundKey,
    config: &HnswSearch,
    distance: f64,
) -> Result<()> {
    let n_keys = config.base_handle.metadata.keys.len();
    if config.bind_field.is_some() {
        let field = if key.1 < n_keys {
            config.base_handle.metadata.keys[key.1].name.clone()
        } else {
            config.base_handle.metadata.non_keys[key.1 - n_keys]
                .name
                .clone()
        };
        tuple.push(DataValue::Str(field));
    }
    if config.bind_field_idx.is_some() {
        tuple.push(if key.2 < 0 {
            DataValue::Null
        } else {
            DataValue::from(key.2 as i64)
        });
    }
    if config.bind_distance.is_some() {
        tuple.push(DataValue::from(distance));
    }
    if config.bind_vector.is_some() {
        let vec = if key.2 < 0 {
            tuple[key.1].clone()
        } else {
            match &tuple[key.1] {
                DataValue::List(v) => v[key.2 as usize].clone(),
                v => bail!("corrupted index value {:?}", v),
            }
        };
        tuple.push(vec);
    }
    Ok(())
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

    fn hnsw_put_vector(
        &mut self,
        tuple: &[DataValue],
        q: &Vector,
        idx: usize,
        subidx: i32,
        manifest: &HnswIndexManifest,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
        vec_cache: &mut VectorCache,
    ) -> Result<()> {
        let tuple_key = &tuple[..orig_table.metadata.keys.len()];
        vec_cache.insert((tuple_key.to_vec(), idx, subidx), q.clone());
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
            self.hnsw_remove_vec(tuple_key, idx, subidx, orig_table, idx_table)?;
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
            let ep_t_key = ep[1..orig_table.metadata.keys.len() + 1].to_vec();
            let ep_idx = ep[orig_table.metadata.keys.len() + 1].get_int().unwrap() as usize;
            let ep_subidx = ep[orig_table.metadata.keys.len() + 2].get_int().unwrap() as i32;
            let ep_key = (ep_t_key, ep_idx, ep_subidx);
            vec_cache.ensure_key(&ep_key, orig_table, self)?;
            let ep_distance = vec_cache.v_dist(q, &ep_key);
            // max queue
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
                    None,
                )?;
            }
            let mut self_tuple_key = Vec::with_capacity(orig_table.metadata.keys.len() * 2 + 5);
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
                self.hnsw_search_level(
                    q,
                    manifest.ef_construction,
                    current_level,
                    orig_table,
                    idx_table,
                    &mut found_nn,
                    vec_cache,
                    None,
                )?;
                // add bidirectional links to the nearest neighbors
                let neighbours = self.hnsw_select_neighbours_heuristic(
                    q,
                    &found_nn,
                    m_max,
                    current_level,
                    manifest,
                    idx_table,
                    orig_table,
                    vec_cache,
                )?;
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
                    let mut out_key = Vec::with_capacity(orig_table.metadata.keys.len() * 2 + 5);
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

                    let mut in_key = Vec::with_capacity(orig_table.metadata.keys.len() * 2 + 5);
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

                    // shrink links if necessary
                    let mut target_self_key =
                        Vec::with_capacity(orig_table.metadata.keys.len() * 2 + 5);
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
                        target_degree = self.hnsw_shrink_neighbour(
                            neighbour,
                            m_max,
                            current_level,
                            manifest,
                            idx_table,
                            orig_table,
                            vec_cache,
                        )?;
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
    fn hnsw_shrink_neighbour(
        &mut self,
        target_key: &CompoundKey,
        m: usize,
        level: i64,
        manifest: &HnswIndexManifest,
        idx_table: &RelationHandle,
        orig_table: &RelationHandle,
        vec_cache: &mut VectorCache,
    ) -> Result<usize> {
        vec_cache.ensure_key(target_key, orig_table, self)?;
        let vec = vec_cache.get_key(target_key).clone();
        let mut candidates = PriorityQueue::new();
        for (neighbour_key, neighbour_dist) in
            self.hnsw_get_neighbours(target_key, level, idx_table, false)?
        {
            candidates.push(neighbour_key, OrderedFloat(neighbour_dist));
        }
        let new_candidates = self.hnsw_select_neighbours_heuristic(
            &vec,
            &candidates,
            m,
            level,
            manifest,
            idx_table,
            orig_table,
            vec_cache,
        )?;
        let mut old_candidate_set = FxHashSet::default();
        for (old, _) in &candidates {
            old_candidate_set.insert(old.clone());
        }
        let mut new_candidate_set = FxHashSet::default();
        for (new, _) in &new_candidates {
            new_candidate_set.insert(new.clone());
        }
        let new_degree = new_candidates.len();
        for (new, Reverse(OrderedFloat(new_dist))) in new_candidates {
            if !old_candidate_set.contains(&new) {
                let mut new_key = Vec::with_capacity(orig_table.metadata.keys.len() * 2 + 5);
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
                let mut old_key = Vec::with_capacity(orig_table.metadata.keys.len() * 2 + 5);
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

        Ok(new_degree)
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
        vec_cache: &mut VectorCache,
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
        while !candidates.is_empty() && ret.len() < m {
            let (cand_key, Reverse(OrderedFloat(cand_dist_to_q))) = candidates.pop().unwrap();
            let mut should_add = true;
            for (existing, _) in ret.iter() {
                vec_cache.ensure_key(&cand_key, orig_table, self)?;
                vec_cache.ensure_key(existing, orig_table, self)?;
                let dist_to_existing = vec_cache.k_dist(existing, &cand_key);
                if distance_is_closer(dist_to_existing, cand_dist_to_q) {
                    should_add = false;
                    break;
                }
            }
            if should_add {
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
    /// One level of the greedy search.
    ///
    /// `admit` separates the two jobs the traversal does. Routing is unconditional: every
    /// neighbour close enough to be worth expanding is expanded, whatever else is true of it.
    /// Qualifying as a *result* is what `admit` governs, and only nodes that reach `found_nn`
    /// are ever returned. Keeping the two apart is what makes a selective predicate widen the
    /// search rather than truncate it: while fewer than `ef` nodes qualify the stopping bound
    /// stays at infinity, so the walk keeps going instead of returning short.
    ///
    /// `None` means every node qualifies, which is what the descent through the upper levels
    /// wants: those levels exist to find an entry point, not to produce answers.
    fn hnsw_search_level(
        &self,
        q: &Vector,
        ef: usize,
        cur_level: i64,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
        found_nn: &mut PriorityQueue<CompoundKey, OrderedFloat<f64>>,
        vec_cache: &mut VectorCache,
        mut admit: Option<&mut HnswAdmit<'_>>,
    ) -> Result<()> {
        let mut visited: FxHashSet<CompoundKey> = FxHashSet::default();
        // min queue
        let mut candidates: PriorityQueue<CompoundKey, Reverse<OrderedFloat<f64>>> =
            PriorityQueue::new();

        for item in found_nn.iter() {
            visited.insert(item.0.clone());
            candidates.push(item.0.clone(), Reverse(*item.1));
        }

        // The entry points arrive as results of the level above, where nothing was asked of
        // them but proximity. Re-test them here, so that a level whose job is to produce
        // answers starts from an answer set it actually vouches for.
        if let Some(admit) = admit.as_deref_mut() {
            let seeds = found_nn
                .iter()
                .map(|(k, d)| (k.clone(), *d))
                .collect::<Vec<_>>();
            found_nn.clear();
            for (key, OrderedFloat(dist)) in seeds {
                if !vec_cache.is_missing(&key) && admit.admits(self, &key, dist, vec_cache)? {
                    found_nn.push(key, OrderedFloat(dist));
                }
            }
        }

        while let Some((candidate, Reverse(OrderedFloat(candidate_dist)))) = candidates.pop() {
            if is_exhausted(found_nn, ef, candidate_dist) {
                break;
            }
            // Fetch all unvisited neighbours' vectors in one batched read
            // (mnestic fork): on the snapshot read path this is one RocksDB
            // MultiGet instead of a serial point get per neighbour.
            let unvisited: Vec<CompoundKey> = self
                .hnsw_get_neighbours(&candidate, cur_level, idx_table, false)?
                .filter_map(|(k, _)| if visited.contains(&k) { None } else { Some(k) })
                .collect();
            vec_cache.ensure_keys(&unvisited, orig_table, self)?;
            for neighbour_key in unvisited {
                if visited.contains(&neighbour_key) {
                    continue;
                }
                // A node with no row has no vector, so it can neither be measured nor
                // returned, and its own edges are unreachable from here.
                if !vec_cache.is_missing(&neighbour_key) {
                    let neighbour_dist = vec_cache.v_dist(q, &neighbour_key);
                    if may_expand(found_nn, ef, neighbour_dist) {
                        candidates
                            .push(neighbour_key.clone(), Reverse(OrderedFloat(neighbour_dist)));
                        // Only nodes that got this far pay for the predicate, so its cost
                        // tracks the churn of the result set rather than the size of the walk.
                        let admitted = match admit.as_deref_mut() {
                            Some(admit) => {
                                admit.admits(self, &neighbour_key, neighbour_dist, vec_cache)?
                            }
                            None => true,
                        };
                        if admitted {
                            found_nn.push(neighbour_key.clone(), OrderedFloat(neighbour_dist));
                            if found_nn.len() > ef {
                                found_nn.pop();
                            }
                        }
                    }
                }
                visited.insert(neighbour_key);
            }
        }

        Ok(())
    }
    fn hnsw_get_neighbours<'b>(
        &'b self,
        cand_key: &'b CompoundKey,
        level: i64,
        idx_handle: &RelationHandle,
        include_deleted: bool,
    ) -> Result<impl Iterator<Item = (CompoundKey, f64)> + 'b> {
        let mut start_tuple = Vec::with_capacity(cand_key.0.len() + 3);
        start_tuple.push(DataValue::from(level));
        start_tuple.extend_from_slice(&cand_key.0);
        start_tuple.push(DataValue::from(cand_key.1 as i64));
        start_tuple.push(DataValue::from(cand_key.2 as i64));
        let key_len = cand_key.0.len();
        let neighbours = idx_handle
            .scan_prefix(self, &start_tuple)
            .map(|res| {
                let tuple = res?;
                let expected_len = 2 * key_len + 8;
                if tuple.len() < expected_len {
                    bail!(
                        "corrupt HNSW edge row: expected at least {expected_len} fields, got {}",
                        tuple.len()
                    );
                }
                let key_idx = tuple[2 * key_len + 3].get_int().ok_or_else(|| {
                    miette!("corrupt HNSW edge row: vector index is not an integer")
                })? as usize;
                let key_subidx = tuple[2 * key_len + 4].get_int().ok_or_else(|| {
                    miette!("corrupt HNSW edge row: vector sub-index is not an integer")
                })? as i32;
                let key_tup = tuple[key_len + 3..2 * key_len + 3].to_vec();
                if key_tup == cand_key.0 {
                    return Ok(None);
                }
                let distance = tuple[2 * key_len + 5]
                    .get_float()
                    .ok_or_else(|| miette!("corrupt HNSW edge row: distance is not numeric"))?;
                if include_deleted {
                    return Ok(Some(((key_tup, key_idx, key_subidx), distance)));
                }
                let is_deleted = tuple[2 * key_len + 7].get_bool().ok_or_else(|| {
                    miette!("corrupt HNSW edge row: deletion marker is not boolean")
                })?;
                Ok((!is_deleted).then_some(((key_tup, key_idx, key_subidx), distance)))
            })
            .filter_map(|res| res.transpose())
            .collect::<Result<Vec<_>>>()?;
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
            for i in 0..orig_table.metadata.keys.len() {
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
        let mut vec_cache = VectorCache {
            cache: FxHashMap::default(),
            rows: FxHashMap::default(),
            missing: FxHashSet::default(),
            lenient: false,
            keep_rows: false,
            distance: manifest.distance,
        };
        self.hnsw_put_inner(
            manifest,
            orig_table,
            idx_table,
            filter,
            stack,
            tuple,
            &mut vec_cache,
        )
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

        let key_len = orig_table.metadata.keys.len();
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

    #[allow(clippy::too_many_arguments)]
    fn hnsw_put_inner(
        &mut self,
        manifest: &HnswIndexManifest,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
        filter: Option<&Vec<Bytecode>>,
        stack: &mut Vec<DataValue>,
        tuple: &[DataValue],
        vec_cache: &mut VectorCache,
    ) -> Result<bool> {
        if let Some(code) = filter {
            if !eval_bytecode_pred(code, tuple, stack, Default::default())? {
                self.hnsw_remove(orig_table, idx_table, tuple)?;
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
        self.hnsw_remove_except(orig_table, idx_table, tuple, &keep)?;
        if extracted_vectors.is_empty() {
            return Ok(false);
        }
        for (vec, idx, sub) in extracted_vectors {
            self.hnsw_put_vector(
                tuple, vec, idx, sub, manifest, orig_table, idx_table, vec_cache,
            )?;
        }
        Ok(true)
    }
    pub(crate) fn hnsw_remove(
        &mut self,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
        tuple: &[DataValue],
    ) -> Result<()> {
        self.hnsw_remove_except(orig_table, idx_table, tuple, &FxHashSet::default())
    }

    fn hnsw_remove_except(
        &mut self,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
        tuple: &[DataValue],
        keep: &FxHashSet<(usize, i32)>,
    ) -> Result<()> {
        let mut prefix = vec![DataValue::from(0)];
        prefix.extend_from_slice(&tuple[0..orig_table.metadata.keys.len()]);
        let candidates: FxHashSet<_> = idx_table
            .scan_prefix(self, &prefix)
            .map(|t| {
                let t = t?;
                let key_len = orig_table.metadata.keys.len();
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
        for (tuple_key, idx, subidx) in candidates {
            if !keep.contains(&(idx, subidx)) {
                self.hnsw_remove_vec(&tuple_key, idx, subidx, orig_table, idx_table)?;
            }
        }
        Ok(())
    }
    fn hnsw_remove_vec(
        &mut self,
        tuple_key: &[DataValue],
        idx: usize,
        subidx: i32,
        orig_table: &RelationHandle,
        idx_table: &RelationHandle,
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
                self.idx_del(idx_table, &self_key_bytes)?;
            } else {
                break;
            }

            let neigbours = self
                .hnsw_get_neighbours(&compound_key, layer, idx_table, true)?
                .collect_vec();
            encountered_singletons |= neigbours.is_empty();
            for (neighbour_key, _) in neigbours {
                // REMARK: this still has some probability of disconnecting the graph.
                // Should we accept that as a consequence of the probabilistic nature of the algorithm?
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
                self.idx_del(idx_table, &in_key_bytes)?;
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
                    .ok_or_else(|| miette!("corrupt HNSW index: neighbour self-row is missing"))?;
                let mut neighbour_val =
                    try_decode_val_only(&neighbour_self_key_bytes, &neighbour_val_bytes)?;
                let degree = neighbour_val
                    .first()
                    .and_then(DataValue::get_float)
                    .ok_or_else(|| miette!("corrupt HNSW self-row: missing numeric degree"))?;
                neighbour_val[0] = DataValue::from(degree - 1.);
                let neighbour_val_bytes_new =
                    idx_table.encode_val_only_for_store(&neighbour_val, Default::default())?;
                self.idx_put(
                    idx_table,
                    &neighbour_self_key_bytes,
                    &neighbour_val_bytes_new,
                )?;
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
                for _ in 0..orig_table.metadata.keys.len() {
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

        let mut vec_cache = VectorCache {
            cache: Default::default(),
            rows: Default::default(),
            missing: Default::default(),
            lenient: true,
            // Only the user filter reads the row itself; radius and liveness work off the key.
            keep_rows: filter_bytecode.is_some(),
            distance: config.manifest.distance,
        };

        // The graph is entered at its highest level. A stack that cannot read the entry
        // point's record cannot measure it either, so walk on to the next one rather than
        // giving up on the whole index.
        let n_keys = config.base_handle.metadata.keys.len();
        let mut entry = None;
        for ep in config.idx_handle.scan_bounded_prefix(
            self,
            &[],
            &[DataValue::from(i64::MIN)],
            &[DataValue::from(1)],
        ) {
            let ep = ep?;
            let Some(ep_idx) = ep[n_keys + 1].get_int() else {
                // this occurs if the index is empty
                return Ok(vec![]);
            };
            let candidate = (
                ep[1..n_keys + 1].to_vec(),
                ep_idx as usize,
                ep[n_keys + 2].get_int().unwrap() as i32,
            );
            let bottom_level = ep[0].get_int().unwrap();
            vec_cache.ensure_key(&candidate, &config.base_handle, self)?;
            if !vec_cache.is_missing(&candidate) {
                entry = Some((candidate, bottom_level));
                break;
            }
        }
        let Some((ep_key, bottom_level)) = entry else {
            return Ok(vec![]);
        };

        let ep_distance = vec_cache.v_dist(&q, &ep_key);
        let mut found_nn = PriorityQueue::new();
        found_nn.push(ep_key, OrderedFloat(ep_distance));
        // The upper levels only route, so nothing is asked of the nodes they pass through.
        for current_level in bottom_level..0 {
            self.hnsw_search_level(
                &q,
                1,
                current_level,
                &config.base_handle,
                &config.idx_handle,
                &mut found_nn,
                &mut vec_cache,
                None,
            )?;
        }
        let mut admit = HnswAdmit::new(config, filter_bytecode);
        let trivial = admit.is_trivial();
        self.hnsw_search_level(
            &q,
            config.ef,
            0,
            &config.base_handle,
            &config.idx_handle,
            &mut found_nn,
            &mut vec_cache,
            if trivial { None } else { Some(&mut admit) },
        )?;
        if found_nn.is_empty() {
            return Ok(vec![]);
        }

        // Everything still here already qualifies, so all but the nearest `k` are about to be
        // thrown away. Drop them before paying to read their rows.
        while found_nn.len() > config.k {
            found_nn.pop();
        }

        let mut ret = Vec::with_capacity(found_nn.len());
        while let Some((cand_key, OrderedFloat(distance))) = found_nn.pop() {
            if let Some(radius) = config.radius {
                if distance > radius {
                    continue;
                }
            }
            // The row was already read during the traversal whenever a predicate needed it.
            let mut row = match vec_cache.rows.get(&cand_key.0) {
                Some(row) => row.clone(),
                None => config
                    .base_handle
                    .get(self, &cand_key.0)?
                    .ok_or_else(|| miette!("corrupted index"))?,
            };
            push_search_bindings(&mut row, &cand_key, config, distance)?;
            ret.push(row);
        }
        ret.reverse();
        ret.truncate(config.k);

        Ok(ret)
    }

    /// The version of the record at `prefix` that is live at `valid_at`, if it has one.
    fn hnsw_live_version(
        &self,
        config: &HnswSearch,
        prefix: &[DataValue],
        valid_at: ValidityTs,
    ) -> Result<Option<DataValue>> {
        let n_keys = config.base_handle.metadata.keys.len();
        match config
            .base_handle
            .skip_scan_bounded_prefix(self, prefix, &[], &[], valid_at)
            .next()
        {
            None => Ok(None),
            Some(newest) => Ok(Some(newest?.swap_remove(n_keys - 1))),
        }
    }
}

#[cfg(test)]
mod tests {
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
}
