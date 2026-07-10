/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::cmp::Ordering;
use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::collections::Bound::Included;
use std::mem;
use std::ops::Bound::Excluded;

use either::{Left, Right};
use itertools::Itertools;
use miette::{bail, ensure, Result};

use crate::data::aggr::Aggregation;
use crate::data::tuple::Tuple;
use crate::data::value::DataValue;

/// A store holding temp data during evaluation of queries.
/// The public interface is used in custom implementations of algorithms/utilities.
#[derive(Default, Debug)]
pub struct RegularTempStore {
    inner: BTreeMap<Tuple, bool>,
}

const EMPTY_TUPLE_REF: &Tuple = &vec![];

impl RegularTempStore {
    pub(crate) fn wrap(self) -> TempStore {
        TempStore::Normal(self)
    }
    /// Tests if a key already exists in the store.
    pub fn exists(&self, key: &Tuple) -> bool {
        self.inner.contains_key(key)
    }

    fn range_iter(
        &self,
        lower: &Tuple,
        upper: &Tuple,
        upper_inclusive: bool,
    ) -> impl Iterator<Item = TupleInIter<'_>> {
        let lower_bound = Included(lower.to_vec());
        let upper_bound = if upper_inclusive {
            Included(upper.to_vec())
        } else {
            Excluded(upper.to_vec())
        };
        self.inner
            .range((lower_bound, upper_bound))
            .map(|(t, skip)| TupleInIter(t, EMPTY_TUPLE_REF, *skip))
    }
    /// Add a tuple to the store
    pub fn put(&mut self, tuple: Tuple) {
        self.inner.insert(tuple, false);
    }
    pub(crate) fn put_with_skip(&mut self, tuple: Tuple) {
        self.inner.insert(tuple, true);
    }
    // returns true if prev is guaranteed to be the same as self after this function call,
    // false if we are not sure.
    pub(crate) fn merge_in(&mut self, prev: &mut Self, mut new: Self) -> bool {
        prev.inner.clear();
        if new.inner.is_empty() {
            return false;
        }
        if self.inner.is_empty() {
            mem::swap(&mut new, self);
            return true;
        }
        for (k, v) in new.inner {
            match self.inner.entry(k) {
                Entry::Vacant(ent) => {
                    prev.inner.insert(ent.key().clone(), v);
                    ent.insert(v);
                }
                Entry::Occupied(mut ent) => {
                    ent.insert(v);
                }
            }
        }
        false
    }
}

#[derive(Debug)]
pub(crate) struct MeetAggrStore {
    inner: BTreeMap<Tuple, Tuple>,
    aggregations: Vec<(Aggregation, Vec<DataValue>)>,
    grouping_len: usize,
}

impl MeetAggrStore {
    pub(crate) fn wrap(self) -> TempStore {
        TempStore::MeetAggr(self)
    }
    pub(crate) fn exists(&self, key: &Tuple) -> bool {
        let truncated = &key[0..self.grouping_len];
        self.inner.contains_key(truncated)
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
    pub(crate) fn new(aggrs: Vec<Option<(Aggregation, Vec<DataValue>)>>) -> Result<Self> {
        let total_key_len = aggrs.len();
        let mut aggregations = aggrs.into_iter().flatten().collect_vec();
        for (aggr, args) in aggregations.iter_mut() {
            aggr.meet_init(args)?;
        }
        let grouping_len = total_key_len - aggregations.len();
        Ok(Self {
            inner: Default::default(),
            aggregations,
            grouping_len,
        })
    }
    // also need to check if value exists beforehand! use the idempotency!
    // need to think this through more carefully.
    pub(crate) fn meet_put(&mut self, tuple: Tuple) -> Result<bool> {
        let (key_part, val_part) = tuple.split_at(self.grouping_len);
        match self.inner.get_mut(key_part) {
            Some(prev_aggr) => {
                let mut changed = false;
                for (i, (aggr_op, _)) in self.aggregations.iter().enumerate() {
                    let op = aggr_op.meet_op.as_ref().unwrap();
                    let this_changed = op.update(&mut prev_aggr[i], &val_part[i])?;
                    #[cfg(debug_assertions)]
                    if this_changed && aggr_op.meet_factory.is_some() {
                        probe_meet_idempotence(op.as_ref(), &prev_aggr[i], &val_part[i])?;
                    }
                    changed |= this_changed;
                }
                Ok(changed)
            }
            None => {
                self.inner.insert(key_part.to_vec(), val_part.to_vec());
                Ok(true)
            }
        }
    }
    fn range_iter(
        &self,
        lower: &Tuple,
        upper: &Tuple,
        upper_inclusive: bool,
    ) -> impl Iterator<Item = TupleInIter<'_>> {
        let lower_key = if lower.len() > self.grouping_len {
            lower[0..self.grouping_len].to_vec()
        } else {
            lower.to_vec()
        };
        let upper_key = if upper.len() > self.grouping_len {
            upper[0..self.grouping_len].to_vec()
        } else {
            upper.to_vec()
        };
        let lower = lower.to_vec();
        let upper = upper.to_vec();
        self.inner
            .range(lower_key..=upper_key)
            .filter_map(move |(k, v)| {
                let ret = TupleInIter(k, v, false);
                if ret.partial_cmp(&lower as &[DataValue]) == Some(Ordering::Less) {
                    None
                } else {
                    match ret.partial_cmp(&upper as &[DataValue]).unwrap() {
                        Ordering::Less => Some(ret),
                        Ordering::Equal => {
                            if upper_inclusive {
                                Some(ret)
                            } else {
                                None
                            }
                        }
                        Ordering::Greater => None,
                    }
                }
            })
    }
    /// returns true if prev is guaranteed to be the same as self after this function call,
    /// false if we are not sure.
    pub(crate) fn merge_in(&mut self, prev: &mut Self, mut new: Self) -> Result<bool> {
        prev.inner.clear();
        if new.inner.is_empty() {
            return Ok(false);
        }
        if self.inner.is_empty() {
            mem::swap(self, &mut new);
            return Ok(true);
        }
        for (k, v) in new.inner {
            match self.inner.entry(k) {
                Entry::Vacant(ent) => {
                    prev.inner.insert(ent.key().clone(), v.clone());
                    ent.insert(v);
                }
                Entry::Occupied(mut ent) => {
                    let mut changed = false;
                    {
                        let target = ent.get_mut();
                        for (i, (aggr_op, _)) in self.aggregations.iter().enumerate() {
                            let op = aggr_op.meet_op.as_ref().unwrap();
                            let this_changed = op.update(&mut target[i], &v[i])?;
                            #[cfg(debug_assertions)]
                            if this_changed && aggr_op.meet_factory.is_some() {
                                probe_meet_idempotence(op.as_ref(), &target[i], &v[i])?;
                            }
                            changed |= this_changed;
                        }
                    }
                    if changed {
                        prev.inner.insert(ent.key().clone(), ent.get().clone());
                    }
                }
            }
        }
        Ok(false)
    }
}

/// The bounded-meet store (mnestic fork, provenance semirings R1): up to k
/// rows per group, each row one candidate "proof" pack. The store owns the
/// k-set mechanics — insert-sorted by the aggregate's total order, dedup on
/// `Ordering::Equal` (the aggregate's `○=`), truncate to k — so the engine
/// controls truncation at every fixpoint step while ⊗ stays ordinary rule-
/// body arithmetic over the rows. Displacement makes this NON-monotone: a
/// row can leave the store, which is why it is a third `AggrKind`, not a
/// meet, and why the evaluator caps epochs (`BOUNDED_MEET_MAX_EPOCHS`).
/// v1 shape: exactly ONE bounded aggregate column, in the last position
/// (validated in `aggr_kind`).
pub(crate) struct BoundedMeetStore {
    /// group key → sorted, deduped, ≤k single-column value tuples
    inner: BTreeMap<Tuple, Vec<Tuple>>,
    op: std::sync::Arc<dyn crate::data::aggr::BoundedMeetAggrObj>,
    k: usize,
    grouping_len: usize,
    /// `true` for a delta store: keep candidates sorted+deduped but do NOT
    /// truncate. (In practice each epoch's out store is itself k-truncating,
    /// so at most k candidates per group reach the delta per epoch — the
    /// unbounded twin is belt-and-braces for the delta contract, not a
    /// load-bearing path.)
    unbounded: bool,
}

impl std::fmt::Debug for BoundedMeetStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundedMeetStore")
            .field("k", &self.k)
            .field("grouping_len", &self.grouping_len)
            .field("inner", &self.inner)
            .finish()
    }
}

impl BoundedMeetStore {
    pub(crate) fn wrap(self) -> TempStore {
        TempStore::BoundedMeet(self)
    }
    pub(crate) fn new(aggrs: Vec<Option<(Aggregation, Vec<DataValue>)>>) -> Result<Self> {
        let total_key_len = aggrs.len();
        let mut bounded = aggrs.into_iter().flatten().collect_vec();
        // aggr_kind validated the shape: exactly one, last position
        debug_assert_eq!(bounded.len(), 1);
        let (aggr, args) = bounded.pop().unwrap();
        let (k, op) = aggr.bounded_meet_init(&args)?;
        Ok(Self {
            inner: Default::default(),
            op: std::sync::Arc::from(op),
            k,
            grouping_len: total_key_len - 1,
            unbounded: false,
        })
    }
    /// An empty twin for the delta slot: same contract, no truncation.
    pub(crate) fn delta_twin(&self) -> Self {
        Self {
            inner: Default::default(),
            op: self.op.clone(),
            k: self.k,
            grouping_len: self.grouping_len,
            unbounded: true,
        }
    }
    pub(crate) fn exists(&self, key: &Tuple) -> bool {
        let truncated = &key[0..self.grouping_len];
        self.inner.contains_key(truncated)
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
    /// Merge one candidate row into its group's k-set. Returns whether the
    /// k-set changed.
    pub(crate) fn meet_put(&mut self, tuple: Tuple) -> Result<bool> {
        let (key_part, val_part) = tuple.split_at(self.grouping_len);
        self.op.validate(&val_part[0])?;
        let op = self.op.clone();
        let (k, unbounded) = (self.k, self.unbounded);
        let entry = self.inner.entry(key_part.to_vec()).or_default();
        match entry.binary_search_by(|held| op.cmp_candidates(&held[0], &val_part[0])) {
            // Equal under the aggregate's order = duplicate under its ○=
            Ok(_) => Ok(false),
            Err(pos) => {
                if !unbounded && pos >= k {
                    // worse than the group's k-th best: not a change
                    return Ok(false);
                }
                entry.insert(pos, val_part.to_vec());
                if !unbounded && entry.len() > k {
                    entry.pop();
                }
                Ok(true)
            }
        }
    }
    fn range_iter(
        &self,
        lower: &Tuple,
        upper: &Tuple,
        upper_inclusive: bool,
    ) -> impl Iterator<Item = TupleInIter<'_>> {
        let lower_key = if lower.len() > self.grouping_len {
            lower[0..self.grouping_len].to_vec()
        } else {
            lower.to_vec()
        };
        let upper_key = if upper.len() > self.grouping_len {
            upper[0..self.grouping_len].to_vec()
        } else {
            upper.to_vec()
        };
        let lower = lower.to_vec();
        let upper = upper.to_vec();
        self.inner
            .range(lower_key..=upper_key)
            .flat_map(|(k, vs)| vs.iter().map(move |v| TupleInIter(k, v, false)))
            .filter(move |ret| {
                if ret.partial_cmp(&lower as &[DataValue]) == Some(Ordering::Less) {
                    return false;
                }
                match ret.partial_cmp(&upper as &[DataValue]).unwrap() {
                    Ordering::Less => true,
                    Ordering::Equal => upper_inclusive,
                    Ordering::Greater => false,
                }
            })
    }
    /// Returns true if prev is guaranteed to be the same as self after this
    /// function call, false if we are not sure. `prev` collects the
    /// candidates that CHANGED a k-set — the delta for the next epoch.
    pub(crate) fn merge_in(&mut self, prev: &mut Self, mut new: Self) -> Result<bool> {
        prev.inner.clear();
        prev.unbounded = true;
        if new.inner.is_empty() {
            return Ok(false);
        }
        if self.inner.is_empty() {
            mem::swap(&mut self.inner, &mut new.inner);
            return Ok(true);
        }
        for (k, vs) in new.inner {
            for v in vs {
                let mut row = k.clone();
                row.extend(v.iter().cloned());
                if self.meet_put(row)? {
                    let mut prev_row = k.clone();
                    prev_row.extend(v);
                    prev.meet_put(prev_row)?;
                }
            }
        }
        Ok(false)
    }
}

/// The dominance bounded-meet store (mnestic fork, spec
/// `docs/specs/antichain-bounded-meet.md`): per group, the set of candidates
/// not dominated by any other — the antichain / Pareto frontier under a
/// registered strict partial order. The insert is BNL in-buffer maintenance:
/// structural-equality dedup first (○= is `DataValue` equality), reject if
/// any survivor dominates the newcomer (sound without checking what the
/// newcomer dominates, by transitivity + the antichain invariant), else
/// evict everything the newcomer dominates and insert. Survivor vectors are
/// kept in `DataValue` (memcmp) order so dedup is a binary search and
/// emission order is canonical rather than arrival-dependent. Like
/// `BoundedMeetStore`, displacement makes this non-monotone: it rides
/// `AggrKind::BoundedMeet` and the evaluator's epoch cap. `max_survivors`
/// guards store growth: overflow is a loud error, never a silent truncation
/// (an antichain has no canonical k-subset). The bail fires on the RUNNING
/// antichain, so whether a run bails can depend on candidate arrival order —
/// the confluence guarantee covers the output values of successful runs.
/// The asymmetry probe runs on reject-direction comparisons; that coverage
/// suffices to keep a symmetric pair from ever corrupting the set (a
/// symmetric pair is caught in the reject loop before any eviction).
pub(crate) struct DominanceMeetStore {
    /// group key → memcmp-sorted, deduped survivor single-column tuples
    inner: BTreeMap<Tuple, Vec<Tuple>>,
    reg: crate::data::aggr::RegisteredBoundedMeet,
    aggr_name: &'static str,
    grouping_len: usize,
    /// `true` for a delta store: equality-dedup only — no dominance pruning,
    /// no cap. The delta must stay a superset of what may still change the
    /// total store, mirroring `BoundedMeetStore`'s unbounded twin.
    dedup_only: bool,
    /// Per-candidate operand validator for a built-in native-dominance
    /// aggregate (mnestic fork — `pareto_min`/`pareto_max`). `None` for a
    /// host-registered dominance, whose closure owns its operand shape. Kept
    /// here rather than on the public `RegisteredBoundedMeet` so that struct's
    /// API is unchanged. `dominates` returns `bool` and cannot report a
    /// malformed operand; this does, loudly.
    validate: Option<fn(&DataValue) -> Result<()>>,
}

impl std::fmt::Debug for DominanceMeetStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DominanceMeetStore")
            .field("aggr_name", &self.aggr_name)
            .field("max_survivors", &self.reg.max_survivors)
            .field("grouping_len", &self.grouping_len)
            .field("inner", &self.inner)
            .finish()
    }
}

impl DominanceMeetStore {
    pub(crate) fn new(aggrs: Vec<Option<(Aggregation, Vec<DataValue>)>>) -> Result<Self> {
        let total_key_len = aggrs.len();
        let mut bounded = aggrs.into_iter().flatten().collect_vec();
        // aggr_kind validated the shape: exactly one, last position
        debug_assert_eq!(bounded.len(), 1);
        let (aggr, args) = bounded.pop().unwrap();
        // belt-and-braces: the parser already rejects call-site args
        ensure!(
            args.is_empty(),
            "registered bounded-meet aggregate '{}' takes no arguments (its cap is set at registration)",
            aggr.name
        );
        let reg = aggr
            .bounded_dominance
            .clone()
            .expect("DominanceMeetStore built for a non-dominance aggregate");
        Ok(Self {
            inner: Default::default(),
            reg,
            aggr_name: aggr.name,
            grouping_len: total_key_len - 1,
            dedup_only: false,
            validate: crate::data::aggr::builtin_skyline_validator(aggr.name),
        })
    }
    /// An empty twin for the delta slot: equality-dedup only.
    pub(crate) fn delta_twin(&self) -> Self {
        Self {
            inner: Default::default(),
            reg: self.reg.clone(),
            aggr_name: self.aggr_name,
            grouping_len: self.grouping_len,
            dedup_only: true,
            validate: self.validate,
        }
    }
    pub(crate) fn exists(&self, key: &Tuple) -> bool {
        let truncated = &key[0..self.grouping_len];
        self.inner.contains_key(truncated)
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
    /// Merge one candidate into its group's antichain. Returns whether the
    /// survivor set changed.
    pub(crate) fn meet_put(&mut self, tuple: Tuple) -> Result<bool> {
        let (key_part, val_part) = tuple.split_at(self.grouping_len);
        let c = &val_part[0];
        // mnestic fork — built-in native-dominance aggregates (`pareto_*`)
        // validate the operand loudly here, before any dedup/dominance work:
        // `dominates` returns `bool` and cannot report a malformed operand.
        // This runs on the dedup_only delta store too, so no unvalidated
        // candidate can slip through to output. Host-registered dominances
        // carry no validator (`None`) and keep their trust-the-closure contract.
        if let Some(validate) = self.validate {
            validate(c)?;
        }
        let entry = self.inner.entry(key_part.to_vec()).or_default();
        // memcmp-order binary search: structural-equality dedup + insertion pos
        let pos = match entry.binary_search_by(|held| held[0].cmp(c)) {
            Ok(_) => return Ok(false),
            Err(pos) => pos,
        };
        if self.dedup_only {
            entry.insert(pos, val_part.to_vec());
            return Ok(true);
        }
        let dom = &self.reg.dominates;
        #[cfg(debug_assertions)]
        if dom(c, c) {
            bail!(
                "dominance for bounded-meet aggregate '{}' violates irreflexivity: dominates(x, x) is true",
                self.aggr_name
            );
        }
        for held in entry.iter() {
            if dom(&held[0], c) {
                #[cfg(debug_assertions)]
                if dom(c, &held[0]) {
                    bail!(
                        "dominance for bounded-meet aggregate '{}' violates asymmetry: dominates(a, b) and dominates(b, a) both hold",
                        self.aggr_name
                    );
                }
                // a survivor dominates the newcomer: by transitivity the
                // newcomer cannot dominate any survivor — reject, unchanged
                return Ok(false);
            }
        }
        entry.retain(|held| !dom(c, &held[0]));
        let pos = entry
            .binary_search_by(|held| held[0].cmp(c))
            .expect_err("deduped candidate reappeared after retain");
        entry.insert(pos, val_part.to_vec());
        if entry.len() > self.reg.max_survivors {
            bail!(
                "bounded-meet aggregate '{}' exceeded max_survivors = {}: the group's non-dominated set does not fit the registered resource guard (raise it, strengthen the dominance, or aggregate a coarser candidate)",
                self.aggr_name,
                self.reg.max_survivors
            );
        }
        Ok(true)
    }
    fn range_iter(
        &self,
        lower: &Tuple,
        upper: &Tuple,
        upper_inclusive: bool,
    ) -> impl Iterator<Item = TupleInIter<'_>> {
        let lower_key = if lower.len() > self.grouping_len {
            lower[0..self.grouping_len].to_vec()
        } else {
            lower.to_vec()
        };
        let upper_key = if upper.len() > self.grouping_len {
            upper[0..self.grouping_len].to_vec()
        } else {
            upper.to_vec()
        };
        let lower = lower.to_vec();
        let upper = upper.to_vec();
        self.inner
            .range(lower_key..=upper_key)
            .flat_map(|(k, vs)| vs.iter().map(move |v| TupleInIter(k, v, false)))
            .filter(move |ret| {
                if ret.partial_cmp(&lower as &[DataValue]) == Some(Ordering::Less) {
                    return false;
                }
                match ret.partial_cmp(&upper as &[DataValue]).unwrap() {
                    Ordering::Less => true,
                    Ordering::Equal => upper_inclusive,
                    Ordering::Greater => false,
                }
            })
    }
    /// Returns true if prev is guaranteed to be the same as self after this
    /// function call, false if we are not sure. `prev` collects the
    /// candidates that CHANGED a survivor set — the delta for the next epoch.
    pub(crate) fn merge_in(&mut self, prev: &mut Self, mut new: Self) -> Result<bool> {
        prev.inner.clear();
        prev.dedup_only = true;
        if new.inner.is_empty() {
            return Ok(false);
        }
        if self.inner.is_empty() && new.dedup_only == self.dedup_only {
            mem::swap(&mut self.inner, &mut new.inner);
            return Ok(true);
        }
        for (k, vs) in new.inner {
            for v in vs {
                let mut row = k.clone();
                row.extend(v.iter().cloned());
                if self.meet_put(row)? {
                    let mut prev_row = k.clone();
                    prev_row.extend(v);
                    prev.meet_put(prev_row)?;
                }
            }
        }
        Ok(false)
    }
}

#[derive(Debug)]
pub(crate) enum TempStore {
    Normal(RegularTempStore),
    MeetAggr(MeetAggrStore),
    BoundedMeet(BoundedMeetStore),
    DominanceMeet(DominanceMeetStore),
}

impl TempStore {
    /// Build the epoch-output store for a bounded-meet rule: a
    /// `DominanceMeetStore` for a registered dominance aggregate, else the
    /// builtin `BoundedMeetStore` (mnestic fork, antichain-bounded-meet spec).
    pub(crate) fn new_bounded(aggrs: Vec<Option<(Aggregation, Vec<DataValue>)>>) -> Result<Self> {
        let is_dominance = aggrs
            .iter()
            .flatten()
            .any(|(a, _)| a.bounded_dominance.is_some());
        Ok(if is_dominance {
            TempStore::DominanceMeet(DominanceMeetStore::new(aggrs)?)
        } else {
            TempStore::BoundedMeet(BoundedMeetStore::new(aggrs)?)
        })
    }
    /// Merge one candidate into a bounded-meet-category store.
    pub(crate) fn bounded_meet_put(&mut self, tuple: Tuple) -> Result<bool> {
        match self {
            TempStore::BoundedMeet(b) => b.meet_put(tuple),
            TempStore::DominanceMeet(d) => d.meet_put(tuple),
            _ => unreachable!("bounded_meet_put on a non-bounded store"),
        }
    }
    fn exists(&self, key: &Tuple) -> bool {
        match self {
            TempStore::Normal(n) => n.exists(key),
            TempStore::MeetAggr(m) => m.exists(key),
            TempStore::BoundedMeet(b) => b.exists(key),
            TempStore::DominanceMeet(d) => d.exists(key),
        }
    }
    fn range_iter(
        &self,
        lower: &Tuple,
        upper: &Tuple,
        upper_inclusive: bool,
    ) -> impl Iterator<Item = TupleInIter<'_>> {
        match self {
            TempStore::Normal(n) => Left(n.range_iter(lower, upper, upper_inclusive)),
            TempStore::MeetAggr(m) => Right(Left(m.range_iter(lower, upper, upper_inclusive))),
            TempStore::BoundedMeet(b) => {
                Right(Right(Left(b.range_iter(lower, upper, upper_inclusive))))
            }
            TempStore::DominanceMeet(d) => {
                Right(Right(Right(d.range_iter(lower, upper, upper_inclusive))))
            }
        }
    }
    fn is_empty(&self) -> bool {
        match self {
            TempStore::Normal(n) => n.inner.is_empty(),
            TempStore::MeetAggr(m) => m.inner.is_empty(),
            TempStore::BoundedMeet(b) => b.inner.is_empty(),
            TempStore::DominanceMeet(d) => d.inner.is_empty(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct EpochStore {
    total: TempStore,
    delta: TempStore,
    use_total_for_delta: bool,
    pub(crate) arity: usize,
}

impl EpochStore {
    pub(crate) fn exists(&self, key: &Tuple) -> bool {
        self.total.exists(key)
    }
    pub(crate) fn new_normal(arity: usize) -> Self {
        Self {
            total: TempStore::Normal(RegularTempStore::default()),
            delta: TempStore::Normal(RegularTempStore::default()),
            use_total_for_delta: true,
            arity,
        }
    }
    pub(crate) fn new_meet(aggrs: &[Option<(Aggregation, Vec<DataValue>)>]) -> Result<Self> {
        Ok(Self {
            total: TempStore::MeetAggr(MeetAggrStore::new(aggrs.to_vec())?),
            delta: TempStore::MeetAggr(MeetAggrStore::new(aggrs.to_vec())?),
            use_total_for_delta: true,
            arity: aggrs.len(),
        })
    }
    /// provenance semirings R1: total is k-bounded; the delta twin is
    /// sorted/deduped but unbounded (one epoch may surface > k candidates).
    pub(crate) fn new_bounded_meet(
        aggrs: &[Option<(Aggregation, Vec<DataValue>)>],
    ) -> Result<Self> {
        // antichain-bounded-meet spec: a registered dominance aggregate gets
        // the antichain store; the builtin (min_cost_k) keeps the k-set store.
        let is_dominance = aggrs
            .iter()
            .flatten()
            .any(|(a, _)| a.bounded_dominance.is_some());
        if is_dominance {
            let total = DominanceMeetStore::new(aggrs.to_vec())?;
            let delta = total.delta_twin();
            return Ok(Self {
                total: TempStore::DominanceMeet(total),
                delta: TempStore::DominanceMeet(delta),
                use_total_for_delta: true,
                arity: aggrs.len(),
            });
        }
        let total = BoundedMeetStore::new(aggrs.to_vec())?;
        let delta = total.delta_twin();
        Ok(Self {
            total: TempStore::BoundedMeet(total),
            delta: TempStore::BoundedMeet(delta),
            use_total_for_delta: true,
            arity: aggrs.len(),
        })
    }
    pub(crate) fn merge_in(&mut self, new: TempStore) -> Result<()> {
        match (&mut self.total, &mut self.delta, new) {
            (TempStore::Normal(total), TempStore::Normal(prev), TempStore::Normal(new)) => {
                self.use_total_for_delta = total.merge_in(prev, new);
            }
            (TempStore::MeetAggr(total), TempStore::MeetAggr(prev), TempStore::MeetAggr(new)) => {
                self.use_total_for_delta = total.merge_in(prev, new)?;
            }
            (
                TempStore::BoundedMeet(total),
                TempStore::BoundedMeet(prev),
                TempStore::BoundedMeet(new),
            ) => {
                self.use_total_for_delta = total.merge_in(prev, new)?;
            }
            (
                TempStore::DominanceMeet(total),
                TempStore::DominanceMeet(prev),
                TempStore::DominanceMeet(new),
            ) => {
                self.use_total_for_delta = total.merge_in(prev, new)?;
            }
            _ => unreachable!(),
        }
        Ok(())
    }
    pub(crate) fn has_delta(&self) -> bool {
        if self.use_total_for_delta {
            !self.total.is_empty()
        } else {
            !self.delta.is_empty()
        }
    }
    pub(crate) fn range_iter(
        &self,
        lower: &Tuple,
        upper: &Tuple,
        upper_inclusive: bool,
    ) -> impl Iterator<Item = TupleInIter<'_>> {
        self.total.range_iter(lower, upper, upper_inclusive)
    }
    pub(crate) fn delta_range_iter(
        &self,
        lower: &Tuple,
        upper: &Tuple,
        upper_inclusive: bool,
    ) -> impl Iterator<Item = TupleInIter<'_>> {
        if self.use_total_for_delta {
            self.total.range_iter(lower, upper, upper_inclusive)
        } else {
            self.delta.range_iter(lower, upper, upper_inclusive)
        }
    }
    pub(crate) fn prefix_iter(&self, prefix: &Tuple) -> impl Iterator<Item = TupleInIter<'_>> {
        let mut upper = prefix.to_vec();
        upper.push(DataValue::Bot);
        self.range_iter(prefix, &upper, true)
    }
    pub(crate) fn delta_prefix_iter(
        &self,
        prefix: &Tuple,
    ) -> impl Iterator<Item = TupleInIter<'_>> {
        let mut upper = prefix.to_vec();
        upper.push(DataValue::Bot);
        self.delta_range_iter(prefix, &upper, true)
    }
    pub(crate) fn all_iter(&self) -> impl Iterator<Item = TupleInIter<'_>> {
        self.prefix_iter(&vec![])
    }
    pub(crate) fn delta_all_iter(&self) -> impl Iterator<Item = TupleInIter<'_>> {
        self.delta_prefix_iter(&vec![])
    }
    pub(crate) fn early_returned_iter(&self) -> impl Iterator<Item = TupleInIter<'_>> {
        self.all_iter().filter(|t| !t.should_skip())
    }
}

#[derive(Copy, Clone)]
pub(crate) struct TupleInIter<'a>(&'a Tuple, &'a Tuple, bool);

impl<'a> TupleInIter<'a> {
    pub(crate) fn get(self, idx: usize) -> &'a DataValue {
        self.0
            .get(idx)
            .unwrap_or_else(|| self.1.get(idx - self.0.len()).unwrap())
    }
    fn should_skip(&self) -> bool {
        self.2
    }
    pub(crate) fn into_tuple(self) -> Tuple {
        self.into_iter().cloned().collect_vec()
    }
}

impl<'a> IntoIterator for TupleInIter<'a> {
    type Item = &'a DataValue;
    type IntoIter = TupleInIterIterator<'a>;

    fn into_iter(self) -> Self::IntoIter {
        TupleInIterIterator {
            inner: self,
            idx: 0,
        }
    }
}

pub(crate) struct TupleInIterIterator<'a> {
    inner: TupleInIter<'a>,
    idx: usize,
}

impl PartialEq for TupleInIter<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.into_iter().eq(*other)
    }
}

impl Eq for TupleInIter<'_> {}

impl Ord for TupleInIter<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.into_iter().cmp(*other)
    }
}

impl PartialOrd for TupleInIter<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq<[DataValue]> for TupleInIter<'_> {
    fn eq(&self, other: &'_ [DataValue]) -> bool {
        self.into_iter().eq(other.iter())
    }
}

impl PartialOrd<[DataValue]> for TupleInIter<'_> {
    fn partial_cmp(&self, other: &'_ [DataValue]) -> Option<Ordering> {
        self.into_iter().partial_cmp(other.iter())
    }
}

impl<'a> Iterator for TupleInIterIterator<'a> {
    type Item = &'a DataValue;

    fn next(&mut self) -> Option<Self::Item> {
        let ret = match self.inner.0.get(self.idx) {
            Some(d) => d,
            None => self.inner.1.get(self.idx - self.inner.0.len())?,
        };
        self.idx += 1;
        Some(ret)
    }
}

/// Debug-build probe (mnestic fork, semirings R0b), CUSTOM aggregates only
/// (some builtins are convergence-safe but not probe-stable — intersection
/// normalizes List→Set on re-entry, so `again == current` can fail on a
/// List-valued first contact): after a ⊕ that reported a
/// change, re-applying the same operand must be a no-op — the meet path
/// presumes an idempotent semilattice ("use the idempotency!"). Catches a
/// non-absorptive ⊕ registered with `is_meet = true` loudly on encountered
/// values instead of silently non-terminating; the law itself remains the
/// registrant's obligation.
#[cfg(debug_assertions)]
fn probe_meet_idempotence(
    op: &dyn crate::data::aggr::MeetAggrObj,
    current: &crate::data::value::DataValue,
    operand: &crate::data::value::DataValue,
) -> miette::Result<()> {
    let mut again = current.clone();
    let changed_again = op.update(&mut again, operand)?;
    debug_assert!(
        !changed_again && &again == current,
        "non-idempotent meet aggregate: re-applying an operand changed the value again \
         (a custom aggregate registered with is_meet=true must be an absorptive \
         semilattice operation)"
    );
    Ok(())
}
