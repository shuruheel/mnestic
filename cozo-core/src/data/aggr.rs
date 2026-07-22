/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Debug, Formatter};

use miette::{bail, ensure, miette, Result};
use rand::prelude::*;

use crate::data::value::{DataValue, Num};

pub struct Aggregation {
    pub name: &'static str,
    pub is_meet: bool,
    /// A *bounded-meet* aggregate (mnestic fork, provenance semirings R1):
    /// keeps up to k rows per group instead of one — the store owns the
    /// sort/dedup/truncate, the rows flow through recursion as ordinary
    /// tuples (⊗ stays body arithmetic), and the evaluator caps epochs
    /// because displacement makes convergence a bound, not a guarantee.
    /// Mutually exclusive with `is_meet`.
    pub is_bounded_meet: bool,
    pub meet_op: Option<Box<dyn MeetAggrObj>>,
    pub normal_op: Option<Box<dyn NormalAggrObj>>,
    /// Factory for a user-registered aggregate (mnestic fork, provenance
    /// semirings R0b): builds the ⊕ operator. `None` for every builtin — the
    /// `define_aggr!` const path is unchanged. Cloned with the Aggregation
    /// (programs are cloned; the ops boxes are not).
    pub meet_factory: Option<std::sync::Arc<dyn Fn() -> Box<dyn MeetAggrObj> + Send + Sync>>,
    /// A user-registered DOMINANCE bounded-meet (mnestic fork, spec
    /// `docs/specs/antichain-bounded-meet.md`): keeps the non-dominated
    /// (antichain / skyline) set per group instead of the k cheapest.
    /// `None` for every builtin and every R0b registered meet. When `Some`,
    /// `is_bounded_meet` is true and the store is a `DominanceMeetStore`.
    pub bounded_dominance: Option<RegisteredBoundedMeet>,
}

impl Clone for Aggregation {
    fn clone(&self) -> Self {
        Self {
            name: self.name,
            is_meet: self.is_meet,
            is_bounded_meet: self.is_bounded_meet,
            meet_op: None,
            normal_op: None,
            meet_factory: self.meet_factory.clone(),
            bounded_dominance: self.bounded_dominance.clone(),
        }
    }
}

/// The candidate contract of one bounded-meet aggregate (provenance
/// semirings R1). The store owns the k-set mechanics (insert-sorted, dedup,
/// truncate); the object owns what a candidate IS: validation and the total
/// order. `cmp_candidates` must be total and return `Equal` ONLY for
/// candidates that are duplicates under the aggregate's `○=` equivalence —
/// `Equal` is how the store deduplicates.
pub trait BoundedMeetAggrObj: Send + Sync {
    fn validate(&self, value: &DataValue) -> Result<()>;
    fn cmp_candidates(&self, a: &DataValue, b: &DataValue) -> std::cmp::Ordering;
}

/// `min_cost_k([payload, cost], k)` — the k lowest-cost candidates per
/// group, each kept as its own output row. The direct generalization of
/// `min_cost` (same `[payload, cost]` pack shape); ties order by the whole
/// pack for determinism, exact-duplicate packs collapse.
pub(crate) struct BoundedMinCostK;

impl BoundedMeetAggrObj for BoundedMinCostK {
    fn validate(&self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::List(l) => {
                ensure!(
                    l.len() == 2,
                    "'min_cost_k' requires a list of exactly two items [payload, cost] as argument"
                );
                ensure!(
                    l[1].get_float().is_some(),
                    "'min_cost_k' cost must be numeric"
                );
                Ok(())
            }
            v => bail!("cannot compute 'min_cost_k' on {:?}", v),
        }
    }
    fn cmp_candidates(&self, a: &DataValue, b: &DataValue) -> std::cmp::Ordering {
        let cost = |v: &DataValue| match v {
            DataValue::List(l) => l[1].get_float().unwrap_or(f64::INFINITY),
            _ => f64::INFINITY,
        };
        cost(a).total_cmp(&cost(b)).then_with(|| a.cmp(b))
    }
}

/// A user-registered aggregate (mnestic fork, provenance semirings R0b): a
/// declared `(⊕, 0̄, is_meet)` — ⊗ stays ordinary rule-body arithmetic, exactly
/// as `min_cost` uses it. Registered on a `Db` via `register_custom_aggr`;
/// in-memory and `Db`-scoped (no persistence — that is R2). Contract for the
/// factory and the produced `MeetAggrObj`: **must not panic** (there is no
/// `catch_unwind` in the engine — a panic unwinds into the host) and the
/// factory must be cheap (it runs O(rules × epochs) per query). If
/// `is_meet` is true the ⊕ MUST be an absorptive semilattice operation
/// (idempotent, commutative, associative) — the stratifier admits it into
/// recursion on that assertion; a debug-build probe re-applies operands to
/// catch violations on encountered values, but the law is the registrant's
/// obligation.
#[derive(Clone)]
pub struct RegisteredAggr {
    pub is_meet: bool,
    pub factory: std::sync::Arc<dyn Fn() -> Box<dyn MeetAggrObj> + Send + Sync>,
}

/// A user-registered dominance bounded-meet aggregate (mnestic fork, spec
/// `docs/specs/antichain-bounded-meet.md`): the head form `name(operand)`
/// keeps, per group, the set of operands not dominated by any other — the
/// antichain / Pareto frontier under `dominates` — each survivor emitted as
/// its own output row, exactly like `min_cost_k` rows.
///
/// Registrant contract (the caller's law; see the spec §3.4):
/// - `dominates` MUST be a strict partial order — irreflexive and transitive
///   (hence asymmetric) — and pure (no interior mutability, clock, or RNG).
///   Debug builds probe irreflexivity and asymmetry on encountered values
///   and error loudly on violation; transitivity is not probed and stays the
///   registrant's obligation. Compose lexicographic tie-breaks only over
///   strict *weak* clauses (incomparability transitive), with a genuinely
///   partial clause in last position only.
/// - `dominates` sees ONLY the aggregated operand: pack every field the
///   predicate inspects into the operand value.
/// - MUST NOT panic (no `catch_unwind` in the engine — a panic unwinds into
///   the host) and should be cheap: it runs O(survivors) per candidate.
/// - `max_survivors` is a mandatory resource guard: a group's non-dominated
///   set exceeding it is a loud error, never a silent truncation (an
///   antichain has no canonical k-subset).
/// - Nothing fingerprints a registered algebra: re-registering a different
///   `dominates` under the same name silently changes what later reads of
///   persisted output mean — version algebra names like schemas.
#[derive(Clone)]
pub struct RegisteredBoundedMeet {
    pub dominates: std::sync::Arc<dyn Fn(&DataValue, &DataValue) -> bool + Send + Sync>,
    pub max_survivors: usize,
}

impl std::fmt::Debug for RegisteredBoundedMeet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegisteredBoundedMeet")
            .field("max_survivors", &self.max_survivors)
            .finish()
    }
}

/// Strict Pareto dominance over a numeric vector (mnestic fork — the built-in
/// skyline aggregates). Under the *minimize* convention `a` dominates `b` iff
/// every component of `a` is `<=` the matching component of `b` and at least
/// one is strictly `<`; the *maximize* convention flips both comparisons.
///
/// This is the componentwise (product) strict partial order: irreflexive
/// (`a` vs `a` has no strictly-better component), asymmetric, and transitive.
/// It therefore satisfies the `dominates` contract natively — unlike a
/// host-supplied closure it cannot violate the order, so it is sound even in
/// the release wheel where the debug irreflexivity/asymmetry probes are
/// compiled out. Non-numeric or differing-length operands are treated as
/// incomparable (return `false`); `pareto_validate` rejects those at ingest,
/// so in practice this only ever sees well-formed equal-length vectors and the
/// guards here are belt-and-braces.
pub(crate) fn pareto_dominates(a: &DataValue, b: &DataValue, minimize: bool) -> bool {
    let (la, lb) = match (a, b) {
        (DataValue::List(la), DataValue::List(lb)) => (la, lb),
        _ => return false,
    };
    if la.is_empty() || la.len() != lb.len() {
        return false;
    }
    let mut strictly_better_somewhere = false;
    for (x, y) in la.iter().zip(lb.iter()) {
        let (xf, yf) = match (x.get_float(), y.get_float()) {
            (Some(x), Some(y)) => (x, y),
            _ => return false,
        };
        // NaN is excluded by pareto_validate, so `<=`/`<` are a total order
        // per component here (±inf compares fine).
        let (weakly_better, strictly_better) = if minimize {
            (xf <= yf, xf < yf)
        } else {
            (xf >= yf, xf > yf)
        };
        if !weakly_better {
            return false;
        }
        strictly_better_somewhere |= strictly_better;
    }
    strictly_better_somewhere
}

/// Operand contract for the built-in skyline aggregates: a non-empty list of
/// numbers, none NaN. Runs once per candidate at the top of
/// `DominanceMeetStore::meet_put` (attached there, by name, so the public
/// `RegisteredBoundedMeet` stays unchanged). Needed because `dominates`
/// returns a bare `bool` and has no channel to report a malformed operand.
pub(crate) fn pareto_validate(value: &DataValue) -> Result<()> {
    match value {
        DataValue::List(l) => {
            ensure!(
                !l.is_empty(),
                "the skyline aggregates 'pareto_min'/'pareto_max' require a non-empty list of numbers as the aggregated value"
            );
            for x in l {
                match x.get_float() {
                    Some(f) => ensure!(
                        !f.is_nan(),
                        "a skyline vector component must be a number, got NaN"
                    ),
                    None => bail!("a skyline vector component must be a number, got {:?}", x),
                }
            }
            Ok(())
        }
        v => bail!(
            "the skyline aggregates 'pareto_min'/'pareto_max' aggregate a list of numbers, got {:?}",
            v
        ),
    }
}

/// If `name` is a built-in skyline aggregate, return its native dominance
/// registration (mnestic fork). Attached in `parse_rule_head_arg`, not in the
/// `const` static `parse_aggr` returns, because an `Arc` cannot live in a
/// `const`. `max_survivors` is `usize::MAX`: a skyline is never truncated
/// (spec §4 — bounding an antichain is a *semantic* reduction, never arrival
/// order), so the store's cap check never fires; the frontier is bounded by
/// the group's own tuple count, like `collect`/`union`. The operand validator
/// travels separately via [`builtin_skyline_validator`], so the public
/// `RegisteredBoundedMeet` needs no extra field.
pub(crate) fn builtin_skyline_dominance(name: &str) -> Option<RegisteredBoundedMeet> {
    let minimize = match name {
        n if n == AGGR_PARETO_MIN.name => true,
        n if n == AGGR_PARETO_MAX.name => false,
        _ => return None,
    };
    Some(RegisteredBoundedMeet {
        dominates: std::sync::Arc::new(move |a: &DataValue, b: &DataValue| {
            pareto_dominates(a, b, minimize)
        }),
        max_survivors: usize::MAX,
    })
}

/// The per-candidate operand validator for a built-in skyline aggregate, keyed
/// by aggregate name (mnestic fork). `None` for a host-registered dominance —
/// its closure owns its operand shape. Attached to the `DominanceMeetStore` at
/// construction, keeping the public `RegisteredBoundedMeet` API unchanged.
pub(crate) fn builtin_skyline_validator(name: &str) -> Option<fn(&DataValue) -> Result<()>> {
    match name {
        n if n == AGGR_PARETO_MIN.name || n == AGGR_PARETO_MAX.name => Some(pareto_validate),
        _ => None,
    }
}

/// The two custom-aggregate registries threaded through parsing as one
/// `Copy` bundle (mnestic fork): R0b meet registrations + dominance
/// bounded-meet registrations. One bundle keeps every pass-through call
/// site untouched when a registry category is added.
#[derive(Clone, Copy)]
pub struct CustomAggrRegistries<'a> {
    pub meet: &'a std::collections::BTreeMap<String, RegisteredAggr>,
    pub bounded: &'a std::collections::BTreeMap<String, RegisteredBoundedMeet>,
}

/// Intern a custom aggregate name to `&'static str`, leaking each DISTINCT
/// name exactly once per process (a bare per-call leak would grow without
/// bound under register→unregister→register cycles).
pub(crate) fn intern_aggr_name(name: &str) -> &'static str {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static INTERNED: Mutex<Option<HashSet<&'static str>>> = Mutex::new(None);
    let mut guard = INTERNED.lock().unwrap();
    let set = guard.get_or_insert_with(HashSet::new);
    match set.get(name) {
        Some(s) => s,
        None => {
            let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
            set.insert(leaked);
            leaked
        }
    }
}

/// Runs a user-registered meet ⊕ as an ordinary (non-recursive) aggregate:
/// state = 0̄, `set` = ⊕ — semantically valid for any meet operation and how
/// the builtin meets already behave. Without this, a custom aggregate in a
/// non-recursive rule would hit `normal_init`'s unreachable!().
struct MeetToNormalAdapter {
    op: Box<dyn MeetAggrObj>,
    state: DataValue,
}

impl NormalAggrObj for MeetToNormalAdapter {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        // changed-bit deliberately unused: the Normal path folds a fixed row
        // set once — there is no semi-naive delta to keep clean
        self.op.update(&mut self.state, value)?;
        Ok(())
    }
    fn get(&self) -> Result<DataValue> {
        Ok(self.state.clone())
    }
}

pub trait NormalAggrObj: Send + Sync {
    fn set(&mut self, value: &DataValue) -> Result<()>;
    fn get(&self) -> Result<DataValue>;
}

pub trait MeetAggrObj: Send + Sync {
    fn init_val(&self) -> DataValue;
    fn update(&self, left: &mut DataValue, right: &DataValue) -> Result<bool>;
}

impl PartialEq for Aggregation {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Debug for Aggregation {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Aggr<{}>", self.name)
    }
}

macro_rules! define_aggr {
    ($name:ident, $is_meet:expr) => {
        define_aggr!($name, $is_meet, false);
    };
    ($name:ident, $is_meet:expr, $is_bounded_meet:expr) => {
        const $name: Aggregation = Aggregation {
            name: stringify!($name),
            is_meet: $is_meet,
            is_bounded_meet: $is_bounded_meet,
            meet_op: None,
            normal_op: None,
            meet_factory: None,
            bounded_dominance: None,
        };
    };
}

// provenance semirings R1: the bounded-meet category's shipped instance
define_aggr!(AGGR_MIN_COST_K, false, true);

// mnestic fork — built-in skyline (Pareto-frontier) aggregates. They ride the
// `DominanceMeetStore` exactly like a host-registered dominance
// (`is_bounded_meet` + `bounded_dominance: Some(..)`), but the dominance is a
// native strict partial order (no closure, no GIL, reachable from every binding
// via plain CozoScript). The `bounded_dominance` `Arc` cannot live in a `const`,
// so `parse_rule_head_arg` attaches it via `builtin_skyline_dominance`; these
// statics exist for name resolution (`parse_aggr`) and reservation (the
// register_* guards). Hand-written rather than `define_aggr!` so `name` is the
// user-facing spelling, giving clean error messages.
const AGGR_PARETO_MIN: Aggregation = Aggregation {
    name: "pareto_min",
    is_meet: false,
    is_bounded_meet: true,
    meet_op: None,
    normal_op: None,
    meet_factory: None,
    bounded_dominance: None,
};
const AGGR_PARETO_MAX: Aggregation = Aggregation {
    name: "pareto_max",
    is_meet: false,
    is_bounded_meet: true,
    meet_op: None,
    normal_op: None,
    meet_factory: None,
    bounded_dominance: None,
};

define_aggr!(AGGR_AND, true);

pub(crate) struct AggrAnd {
    accum: bool,
}

impl Default for AggrAnd {
    fn default() -> Self {
        Self { accum: true }
    }
}

impl NormalAggrObj for AggrAnd {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::Bool(v) => self.accum &= *v,
            v => bail!("cannot compute 'and' for {:?}", v),
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(DataValue::from(self.accum))
    }
}

pub(crate) struct MeetAggrAnd;

impl MeetAggrObj for MeetAggrAnd {
    fn init_val(&self) -> DataValue {
        DataValue::from(true)
    }

    fn update(&self, left: &mut DataValue, right: &DataValue) -> Result<bool> {
        match (left, right) {
            (DataValue::Bool(l), DataValue::Bool(r)) => {
                let old = *l;
                *l &= *r;
                // mnestic fork fix: report whether the value CHANGED (was
                // inverted upstream — a change never propagated through the
                // semi-naive delta, and stable values were kept in it)
                Ok(old != *l)
            }
            (u, v) => bail!("cannot compute 'and' for {:?} and {:?}", u, v),
        }
    }
}

define_aggr!(AGGR_OR, true);

#[derive(Default)]
pub(crate) struct AggrOr {
    accum: bool,
}

impl NormalAggrObj for AggrOr {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::Bool(v) => self.accum |= *v,
            v => bail!("cannot compute 'or' for {:?}", v),
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(DataValue::from(self.accum))
    }
}

pub(crate) struct MeetAggrOr;

impl MeetAggrObj for MeetAggrOr {
    fn init_val(&self) -> DataValue {
        DataValue::from(false)
    }

    fn update(&self, left: &mut DataValue, right: &DataValue) -> Result<bool> {
        match (left, right) {
            (DataValue::Bool(l), DataValue::Bool(r)) => {
                let old = *l;
                *l |= *r;
                // mnestic fork fix: report change, not stability (see 'and')
                Ok(old != *l)
            }
            (u, v) => bail!("cannot compute 'or' for {:?} and {:?}", u, v),
        }
    }
}

define_aggr!(AGGR_UNIQUE, false);

#[derive(Default)]
pub(crate) struct AggrUnique {
    accum: BTreeSet<DataValue>,
}

impl NormalAggrObj for AggrUnique {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        self.accum.insert(value.clone());
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(DataValue::List(self.accum.iter().cloned().collect()))
    }
}

define_aggr!(AGGR_GROUP_COUNT, false);

#[derive(Default)]
pub(crate) struct AggrGroupCount {
    accum: BTreeMap<DataValue, i64>,
}

impl NormalAggrObj for AggrGroupCount {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        let entry = self.accum.entry(value.clone()).or_default();
        *entry += 1;
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(DataValue::List(
            self.accum
                .iter()
                .map(|(k, v)| DataValue::List(vec![k.clone(), DataValue::from(*v)]))
                .collect(),
        ))
    }
}

define_aggr!(AGGR_COUNT_UNIQUE, false);

#[derive(Default)]
pub(crate) struct AggrCountUnique {
    count: i64,
    accum: BTreeSet<DataValue>,
}

impl NormalAggrObj for AggrCountUnique {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        if !self.accum.contains(value) {
            self.accum.insert(value.clone());
            self.count += 1;
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(DataValue::from(self.count))
    }
}

define_aggr!(AGGR_UNION, true);

#[derive(Default)]
pub(crate) struct AggrUnion {
    accum: BTreeSet<DataValue>,
}

impl NormalAggrObj for AggrUnion {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::List(v) => self.accum.extend(v.iter().cloned()),
            v => bail!("cannot compute 'union' for value {:?}", v),
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(DataValue::List(self.accum.iter().cloned().collect()))
    }
}

pub(crate) struct MeetAggrUnion;

impl MeetAggrObj for MeetAggrUnion {
    fn init_val(&self) -> DataValue {
        DataValue::Set(BTreeSet::new())
    }

    fn update(&self, left: &mut DataValue, right: &DataValue) -> Result<bool> {
        loop {
            if let DataValue::List(l) = left {
                let s = l.iter().cloned().collect();
                *left = DataValue::Set(s);
                continue;
            }
            return Ok(match (left, right) {
                (DataValue::Set(l), DataValue::Set(s)) => {
                    let mut inserted = false;
                    for v in s.iter() {
                        inserted |= l.insert(v.clone());
                    }
                    inserted
                }
                (DataValue::Set(l), DataValue::List(s)) => {
                    let mut inserted = false;
                    for v in s.iter() {
                        inserted |= l.insert(v.clone());
                    }
                    inserted
                }
                (_, v) => bail!("cannot compute 'union' for value {:?}", v),
            });
        }
    }
}

define_aggr!(AGGR_INTERSECTION, true);

#[derive(Default)]
pub(crate) struct AggrIntersection {
    accum: Option<BTreeSet<DataValue>>,
}

impl NormalAggrObj for AggrIntersection {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::List(v) => {
                if let Some(accum) = &mut self.accum {
                    let new = accum
                        .intersection(&v.iter().cloned().collect())
                        .cloned()
                        .collect();
                    *accum = new;
                } else {
                    self.accum = Some(v.iter().cloned().collect())
                }
            }
            v => bail!("cannot compute 'intersection' for value {:?}", v),
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        match &self.accum {
            None => Ok(DataValue::List(vec![])),
            Some(l) => Ok(DataValue::List(l.iter().cloned().collect())),
        }
    }
}

pub(crate) struct MeetAggrIntersection;

impl MeetAggrObj for MeetAggrIntersection {
    fn init_val(&self) -> DataValue {
        DataValue::Null
    }

    fn update(&self, left: &mut DataValue, right: &DataValue) -> Result<bool> {
        if *left == DataValue::Null && *right != DataValue::Null {
            *left = right.clone();
            return Ok(true);
        } else if *right == DataValue::Null {
            return Ok(false);
        }
        loop {
            if let DataValue::List(l) = left {
                let s = l.iter().cloned().collect();
                *left = DataValue::Set(s);
                continue;
            }
            return Ok(match (left, right) {
                (DataValue::Set(l), DataValue::Set(s)) => {
                    let old_len = l.len();
                    let new_set = l.intersection(s).cloned().collect::<BTreeSet<_>>();
                    if old_len == new_set.len() {
                        false
                    } else {
                        *l = new_set;
                        true
                    }
                }
                (DataValue::Set(l), DataValue::List(s)) => {
                    let old_len = l.len();
                    let s: BTreeSet<_> = s.iter().cloned().collect();
                    let new_set = l.intersection(&s).cloned().collect::<BTreeSet<_>>();
                    if old_len == new_set.len() {
                        false
                    } else {
                        *l = new_set;
                        true
                    }
                }
                (_, v) => bail!("cannot compute 'union' for value {:?}", v),
            });
        }
    }
}

define_aggr!(AGGR_COLLECT, false);

#[derive(Default)]
pub(crate) struct AggrCollect {
    limit: Option<usize>,
    accum: Vec<DataValue>,
}

impl AggrCollect {
    fn new(limit: usize) -> Self {
        Self {
            limit: Some(limit),
            accum: vec![],
        }
    }
}

impl NormalAggrObj for AggrCollect {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        if let Some(limit) = self.limit {
            if self.accum.len() >= limit {
                return Ok(());
            }
        }
        self.accum.push(value.clone());
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(DataValue::List(self.accum.clone()))
    }
}

define_aggr!(AGGR_INTERVAL_COALESCE, false);

/// mnestic fork (spec: docs/specs/cozoscript-extensions.md §3.4): merge the
/// group's half-open `[start, end)` interval spans into maximal intervals.
/// Adjacent (touching) spans merge, since `[0, 5)` + `[5, 10)` = `[0, 10)`.
/// Ordinary (non-meet) aggregate: equal-valued grouping happens via the rule
/// head's other columns, exactly like `collect`.
#[derive(Default)]
pub(crate) struct AggrIntervalCoalesce {
    accum: Vec<(Num, Num)>,
}

impl NormalAggrObj for AggrIntervalCoalesce {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        // Shared bound validation + NUMERIC comparison (not Num's storage Ord,
        // under which Int(5) and Float(5.0) are never equal) — see functions.rs.
        let (s, e) = crate::data::functions::to_interval(value, "'interval_coalesce'")?;
        self.accum.push((s, e));
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        use crate::data::functions::interval_num_cmp;
        use std::cmp::Ordering::Greater;
        let mut spans = self.accum.clone();
        spans.sort_by(|(s1, e1), (s2, e2)| {
            interval_num_cmp(*s1, *s2).then(interval_num_cmp(*e1, *e2))
        });
        let mut merged: Vec<(Num, Num)> = Vec::with_capacity(spans.len());
        for (s, e) in spans {
            match merged.last_mut() {
                // Overlapping or numerically adjacent: extend the open span.
                Some((_, cur_end)) if interval_num_cmp(s, *cur_end) != Greater => {
                    if interval_num_cmp(e, *cur_end) == Greater {
                        *cur_end = e;
                    }
                }
                _ => merged.push((s, e)),
            }
        }
        Ok(DataValue::List(
            merged
                .into_iter()
                .map(|(s, e)| DataValue::List(vec![DataValue::Num(s), DataValue::Num(e)]))
                .collect(),
        ))
    }
}

define_aggr!(AGGR_CHOICE_RAND, false);

pub(crate) struct AggrChoiceRand {
    count: usize,
    value: DataValue,
}

impl Default for AggrChoiceRand {
    fn default() -> Self {
        Self {
            count: 0,
            value: DataValue::Null,
        }
    }
}

impl NormalAggrObj for AggrChoiceRand {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        self.count += 1;
        let prob = 1. / (self.count as f64);
        let rd = thread_rng().gen::<f64>();
        if rd < prob {
            self.value = value.clone();
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(self.value.clone())
    }
}

define_aggr!(AGGR_COUNT, false);

#[derive(Default)]
pub(crate) struct AggrCount {
    count: i64,
}

impl NormalAggrObj for AggrCount {
    fn set(&mut self, _value: &DataValue) -> Result<()> {
        self.count += 1;
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(DataValue::from(self.count))
    }
}

define_aggr!(AGGR_VARIANCE, false);

#[derive(Default)]
pub(crate) struct AggrVariance {
    count: i64,
    sum: f64,
    sum_sq: f64,
}

impl NormalAggrObj for AggrVariance {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::Num(n) => {
                let f = n.get_float();
                self.sum += f;
                self.sum_sq += f * f;
                self.count += 1;
            }
            v => bail!("cannot compute 'variance': encountered value {:?}", v),
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        let ct = self.count as f64;
        Ok(DataValue::from(
            (self.sum_sq - self.sum * self.sum / ct) / (ct - 1.),
        ))
    }
}

define_aggr!(AGGR_STD_DEV, false);

#[derive(Default)]
pub(crate) struct AggrStdDev {
    count: i64,
    sum: f64,
    sum_sq: f64,
}

impl NormalAggrObj for AggrStdDev {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::Num(n) => {
                let f = n.get_float();
                self.sum += f;
                self.sum_sq += f * f;
                self.count += 1;
            }
            v => bail!("cannot compute 'std_dev': encountered value {:?}", v),
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        let ct = self.count as f64;
        let var = (self.sum_sq - self.sum * self.sum / ct) / (ct - 1.);
        Ok(DataValue::from(var.sqrt()))
    }
}

define_aggr!(AGGR_MEAN, false);

#[derive(Default)]
pub(crate) struct AggrMean {
    count: i64,
    sum: f64,
}

impl NormalAggrObj for AggrMean {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::Num(n) => {
                self.sum += n.get_float();
                self.count += 1;
            }
            v => bail!("cannot compute 'mean': encountered value {:?}", v),
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(DataValue::from(self.sum / (self.count as f64)))
    }
}

define_aggr!(AGGR_SUM, false);

#[derive(Default)]
pub(crate) struct AggrSum {
    sum: f64,
}

impl NormalAggrObj for AggrSum {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::Num(n) => {
                self.sum += n.get_float();
            }
            v => bail!("cannot compute 'sum': encountered value {:?}", v),
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(DataValue::from(self.sum))
    }
}

define_aggr!(AGGR_PRODUCT, false);

pub(crate) struct AggrProduct {
    product: f64,
}

impl Default for AggrProduct {
    fn default() -> Self {
        Self { product: 1.0 }
    }
}

impl NormalAggrObj for AggrProduct {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::Num(n) => {
                self.product *= n.get_float();
            }
            v => bail!("cannot compute 'product': encountered value {:?}", v),
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(DataValue::from(self.product))
    }
}

// --- internal exact-integer sum-of-products (mnestic fork, query factorization
// automatic count rewrite; `query/factorize.rs`) ---------------------------
//
// This aggregate exists ONLY to back the synthesized factorized-count rules. It
// is deliberately NOT exposed in `parse_aggr`: no user script can name it, it is
// injected directly into synthesized `NormalFormInlineRule`s.
//
// Why it is not `sum` + `a * b`:
//   * `AggrSum` accumulates in `f64` — exact only below 2^53, and it changes the
//     head column's type from Int to Float. A count must stay an exact `i64`.
//   * `op_mul` multiplies `i64` with a plain `*` that WRAPS in release builds.
//
// Per row it receives a `List` of the per-component sub-counts (all `Int`),
// forms their product, and adds it to a running total — every step through
// `checked_mul` / `checked_add`. On overflow it returns an error rather than a
// wrapped (silently wrong) value. Because every intermediate per-key product and
// partial sum is bounded above by the final answer, an overflow here implies the
// true count itself exceeds `i64::MAX` (a count that `count()` could never have
// enumerated in the first place), so erroring is strictly safer than the naive
// path's silent wrap.
define_aggr!(AGGR_INT_SUM_PROD, false);

#[derive(Default)]
pub(crate) struct AggrIntSumProd {
    sum: i64,
}

impl NormalAggrObj for AggrIntSumProd {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        let items = match value {
            DataValue::List(l) => l.as_slice(),
            v => bail!("'int_sum_prod' expects a list of integers, got {:?}", v),
        };
        let mut product: i64 = 1;
        for it in items {
            let n = it
                .get_int()
                .ok_or_else(|| miette!("'int_sum_prod' factor is not an integer: {:?}", it))?;
            product = product.checked_mul(n).ok_or_else(|| {
                miette!("'int_sum_prod' overflowed i64 while multiplying factors")
            })?;
        }
        self.sum = self
            .sum
            .checked_add(product)
            .ok_or_else(|| miette!("'int_sum_prod' overflowed i64 while summing products"))?;
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(DataValue::from(self.sum))
    }
}

/// The builtin `count` aggregate, cloned for injection into synthesized rules
/// (mnestic fork, query factorization). Base-case sub-rules count rows in exact
/// `i64` via `AggrCount`.
pub(crate) fn count_aggr() -> Aggregation {
    AGGR_COUNT.clone()
}

/// The stable interned name of the `count` aggregate — used by the factorizer to
/// recognize an eligible `count()` head (and to reject `count_unique`, whose name
/// differs).
pub(crate) fn count_aggr_name() -> &'static str {
    AGGR_COUNT.name
}

/// The internal exact-`i64` sum-of-products aggregate, cloned for injection into
/// synthesized combine rules (mnestic fork, query factorization).
pub(crate) fn int_sum_prod_aggr() -> Aggregation {
    AGGR_INT_SUM_PROD.clone()
}

define_aggr!(AGGR_MIN, true);

pub(crate) struct AggrMin {
    found: DataValue,
}

impl Default for AggrMin {
    fn default() -> Self {
        Self {
            found: DataValue::Null,
        }
    }
}

impl NormalAggrObj for AggrMin {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        if *value == DataValue::Null {
            return Ok(());
        }
        if self.found == DataValue::Null {
            self.found = value.clone();
            return Ok(());
        }
        let f1 = self
            .found
            .get_float()
            .ok_or_else(|| miette!("'min' applied to non-numerical values"))?;
        let f2 = value
            .get_float()
            .ok_or_else(|| miette!("'min' applied to non-numerical values"))?;
        if f1 > f2 {
            self.found = value.clone();
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(self.found.clone())
    }
}

pub(crate) struct MeetAggrMin;

impl MeetAggrObj for MeetAggrMin {
    fn init_val(&self) -> DataValue {
        DataValue::Null
    }

    fn update(&self, left: &mut DataValue, right: &DataValue) -> Result<bool> {
        if *right == DataValue::Null {
            return Ok(false);
        }
        if *left == DataValue::Null {
            *left = right.clone();
            return Ok(true);
        }
        let f1 = left
            .get_float()
            .ok_or_else(|| miette!("'min' applied to non-numerical values"))?;
        let f2 = right
            .get_float()
            .ok_or_else(|| miette!("'min' applied to non-numerical values"))?;

        Ok(if f1 > f2 {
            *left = right.clone();
            true
        } else {
            false
        })
    }
}

define_aggr!(AGGR_MAX, true);

pub(crate) struct AggrMax {
    found: DataValue,
}

impl Default for AggrMax {
    fn default() -> Self {
        Self {
            found: DataValue::Null,
        }
    }
}

impl NormalAggrObj for AggrMax {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        if *value == DataValue::Null {
            return Ok(());
        }
        if self.found == DataValue::Null {
            self.found = value.clone();
            return Ok(());
        }
        let f1 = self
            .found
            .get_float()
            .ok_or_else(|| miette!("'min' applied to non-numerical values"))?;
        let f2 = value
            .get_float()
            .ok_or_else(|| miette!("'min' applied to non-numerical values"))?;
        if f1 < f2 {
            self.found = value.clone();
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(self.found.clone())
    }
}

pub(crate) struct MeetAggrMax;

impl MeetAggrObj for MeetAggrMax {
    fn init_val(&self) -> DataValue {
        DataValue::Null
    }

    fn update(&self, left: &mut DataValue, right: &DataValue) -> Result<bool> {
        if *right == DataValue::Null {
            return Ok(false);
        }
        if *left == DataValue::Null {
            *left = right.clone();
            return Ok(true);
        }
        let f1 = left
            .get_float()
            .ok_or_else(|| miette!("'min' applied to non-numerical values"))?;
        let f2 = right
            .get_float()
            .ok_or_else(|| miette!("'min' applied to non-numerical values"))?;

        Ok(if f1 < f2 {
            *left = right.clone();
            true
        } else {
            false
        })
    }
}

define_aggr!(AGGR_LATEST_BY, false);

pub(crate) struct AggrLatestBy {
    found: DataValue,
    cost: DataValue,
}

impl Default for AggrLatestBy {
    fn default() -> Self {
        Self {
            found: DataValue::Null,
            cost: DataValue::Null,
        }
    }
}

impl NormalAggrObj for AggrLatestBy {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::List(l) => {
                ensure!(
                    l.len() == 2,
                    "'latest_by' requires a list of exactly two items as argument"
                );
                let c = &l[1];
                if *c > self.cost {
                    self.cost = c.clone();
                    self.found = l[0].clone();
                }
                Ok(())
            }
            v => bail!("cannot compute 'latest_by' on {:?}", v),
        }
    }

    fn get(&self) -> Result<DataValue> {
        Ok(self.found.clone())
    }
}

define_aggr!(AGGR_SMALLEST_BY, false);

pub(crate) struct AggrSmallestBy {
    found: DataValue,
    cost: DataValue,
}

impl Default for AggrSmallestBy {
    fn default() -> Self {
        Self {
            found: DataValue::Null,
            cost: DataValue::Null,
        }
    }
}

impl NormalAggrObj for AggrSmallestBy {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::List(l) => {
                ensure!(
                    l.len() == 2,
                    "'smallest_by' requires a list of exactly two items as argument"
                );
                let c = &l[1];
                if self.cost == DataValue::Null || *c < self.cost {
                    self.cost = c.clone();
                    self.found = l[0].clone();
                }
                Ok(())
            }
            v => bail!("cannot compute 'smallest_by' on {:?}", v),
        }
    }

    fn get(&self) -> Result<DataValue> {
        Ok(self.found.clone())
    }
}

define_aggr!(AGGR_MIN_COST, true);

pub(crate) struct AggrMinCost {
    found: DataValue,
    cost: f64,
}

impl Default for AggrMinCost {
    fn default() -> Self {
        Self {
            found: DataValue::Null,
            cost: f64::INFINITY,
        }
    }
}

impl NormalAggrObj for AggrMinCost {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::List(l) => {
                ensure!(
                    l.len() == 2,
                    "'min_cost' requires a list of exactly two items as argument"
                );
                let c = &l[1];
                let cost = c
                    .get_float()
                    .ok_or_else(|| miette!("Cost must be numeric"))?;
                if cost < self.cost {
                    self.cost = cost;
                    self.found = l[0].clone();
                }
                Ok(())
            }
            v => bail!("cannot compute 'min_cost' on {:?}", v),
        }
    }

    fn get(&self) -> Result<DataValue> {
        Ok(DataValue::List(vec![
            self.found.clone(),
            DataValue::from(self.cost),
        ]))
    }
}

pub(crate) struct MeetAggrMinCost;

impl MeetAggrObj for MeetAggrMinCost {
    fn init_val(&self) -> DataValue {
        DataValue::List(vec![DataValue::Null, DataValue::from(f64::INFINITY)])
    }

    fn update(&self, left: &mut DataValue, right: &DataValue) -> Result<bool> {
        Ok(match (left, right) {
            (DataValue::List(prev), DataValue::List(l)) => {
                ensure!(
                    l.len() == 2 && prev.len() == 2,
                    "'min_cost' requires a list of length 2 as argument, got {:?}, {:?}",
                    prev,
                    l
                );
                let cur_cost = l.get(1).unwrap();
                let cur_cost = cur_cost
                    .get_float()
                    .ok_or_else(|| miette!("'min_cost' must have numerical costs"))?;
                let prev_cost = prev.get(1).unwrap();
                let prev_cost = prev_cost
                    .get_float()
                    .ok_or_else(|| miette!("'prev_cost' must have numerical costs"))?;

                if prev_cost <= cur_cost {
                    false
                } else {
                    *prev = l.clone();
                    true
                }
            }
            (u, v) => bail!("cannot compute 'min_cost' on {:?}, {:?}", u, v),
        })
    }
}

define_aggr!(AGGR_SHORTEST, true);

#[derive(Default)]
pub(crate) struct AggrShortest {
    found: Option<Vec<DataValue>>,
}

impl NormalAggrObj for AggrShortest {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::List(l) => {
                match self.found {
                    None => self.found = Some(l.clone()),
                    Some(ref mut found) => {
                        if l.len() < found.len() {
                            *found = l.clone();
                        }
                    }
                }
                Ok(())
            }
            v => bail!("cannot compute 'shortest' on {:?}", v),
        }
    }

    fn get(&self) -> Result<DataValue> {
        Ok(match self.found {
            None => DataValue::Null,
            Some(ref l) => DataValue::List(l.clone()),
        })
    }
}

pub(crate) struct MeetAggrShortest;

impl MeetAggrObj for MeetAggrShortest {
    fn init_val(&self) -> DataValue {
        DataValue::Null
    }

    fn update(&self, left: &mut DataValue, right: &DataValue) -> Result<bool> {
        if *left == DataValue::Null && *right != DataValue::Null {
            *left = right.clone();
            return Ok(true);
        } else if *right == DataValue::Null {
            return Ok(false);
        }
        match (left, right) {
            (DataValue::List(l), DataValue::List(r)) => Ok(if r.len() < l.len() {
                *l = r.clone();
                true
            } else {
                false
            }),
            (l, v) => bail!("cannot compute 'shortest' on {:?} and {:?}", l, v),
        }
    }
}

define_aggr!(AGGR_CHOICE, true);

pub(crate) struct AggrChoice {
    found: DataValue,
}

impl Default for AggrChoice {
    fn default() -> Self {
        Self {
            found: DataValue::Null,
        }
    }
}

impl NormalAggrObj for AggrChoice {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        if self.found == DataValue::Null {
            self.found = value.clone();
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(self.found.clone())
    }
}

pub(crate) struct MeetAggrChoice;

impl MeetAggrObj for MeetAggrChoice {
    fn init_val(&self) -> DataValue {
        DataValue::Null
    }

    fn update(&self, left: &mut DataValue, right: &DataValue) -> Result<bool> {
        Ok(if *left == DataValue::Null && *right != DataValue::Null {
            *left = right.clone();
            true
        } else {
            false
        })
    }
}

define_aggr!(AGGR_BIT_AND, true);

#[derive(Default)]
pub(crate) struct AggrBitAnd {
    res: Vec<u8>,
}

impl NormalAggrObj for AggrBitAnd {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::Bytes(bs) => {
                if self.res.is_empty() {
                    self.res = bs.to_vec();
                } else {
                    ensure!(
                        self.res.len() == bs.len(),
                        "operands of 'bit_and' must have the same lengths, got {:x?} and {:x?}",
                        self.res,
                        bs
                    );
                    for (l, r) in self.res.iter_mut().zip(bs.iter()) {
                        *l &= *r;
                    }
                }
                Ok(())
            }
            v => bail!("cannot apply 'bit_and' to {:?}", v),
        }
    }

    fn get(&self) -> Result<DataValue> {
        Ok(DataValue::Bytes(self.res.clone()))
    }
}

pub(crate) struct MeetAggrBitAnd;

impl MeetAggrObj for MeetAggrBitAnd {
    fn init_val(&self) -> DataValue {
        // NOT the ∧-identity (that would be all-ones of the operand width,
        // which is runtime-determined): the ⊕-identity is lazy, seeded from
        // the first operand by `update`'s is_empty branch. Empty bytes is a
        // sentinel.
        DataValue::Bytes(vec![])
    }

    fn update(&self, left: &mut DataValue, right: &DataValue) -> Result<bool> {
        match (left, right) {
            (DataValue::Bytes(left), DataValue::Bytes(right)) => {
                if left == right {
                    return Ok(false);
                }
                if left.is_empty() {
                    *left = right.clone();
                    return Ok(true);
                }
                ensure!(
                    left.len() == right.len(),
                    "operands of 'bit_and' must have the same lengths, got {:x?} and {:x?}",
                    left,
                    right
                );
                // mnestic fork fix: report whether the value CHANGED (was
                // unconditionally true — a non-changing AND re-entered the
                // semi-naive delta every epoch)
                let mut changed = false;
                for (l, r) in left.iter_mut().zip(right.iter()) {
                    let old = *l;
                    *l &= *r;
                    changed |= old != *l;
                }

                Ok(changed)
            }
            v => bail!("cannot apply 'bit_and' to {:?}", v),
        }
    }
}

define_aggr!(AGGR_BIT_OR, true);

#[derive(Default)]
pub(crate) struct AggrBitOr {
    res: Vec<u8>,
}

impl NormalAggrObj for AggrBitOr {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::Bytes(bs) => {
                if self.res.is_empty() {
                    self.res = bs.to_vec();
                } else {
                    ensure!(
                        self.res.len() == bs.len(),
                        "operands of 'bit_or' must have the same lengths, got {:x?} and {:x?}",
                        self.res,
                        bs
                    );
                    for (l, r) in self.res.iter_mut().zip(bs.iter()) {
                        *l |= *r;
                    }
                }
                Ok(())
            }
            v => bail!("cannot apply 'bit_or' to {:?}", v),
        }
    }

    fn get(&self) -> Result<DataValue> {
        Ok(DataValue::Bytes(self.res.clone()))
    }
}

pub(crate) struct MeetAggrBitOr;

impl MeetAggrObj for MeetAggrBitOr {
    fn init_val(&self) -> DataValue {
        // NOT the ∨-identity (all-zeros of the runtime-determined operand
        // width): like `bit_and`, the identity is lazy — seeded from the
        // first operand by `update`'s is_empty branch.
        DataValue::Bytes(vec![])
    }

    fn update(&self, left: &mut DataValue, right: &DataValue) -> Result<bool> {
        match (left, right) {
            (DataValue::Bytes(left), DataValue::Bytes(right)) => {
                if left == right {
                    return Ok(false);
                }
                if left.is_empty() {
                    *left = right.clone();
                    return Ok(true);
                }
                ensure!(
                    left.len() == right.len(),
                    "operands of 'bit_or' must have the same lengths, got {:x?} and {:x?}",
                    left,
                    right
                );
                // mnestic fork fix: report whether the value CHANGED (was
                // unconditionally true — a non-changing OR re-entered the
                // semi-naive delta every epoch)
                let mut changed = false;
                for (l, r) in left.iter_mut().zip(right.iter()) {
                    let old = *l;
                    *l |= *r;
                    changed |= old != *l;
                }

                Ok(changed)
            }
            v => bail!("cannot apply 'bit_or' to {:?}", v),
        }
    }
}

define_aggr!(AGGR_BIT_XOR, false);

#[derive(Default)]
pub(crate) struct AggrBitXor {
    res: Vec<u8>,
}

impl NormalAggrObj for AggrBitXor {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::Bytes(bs) => {
                if self.res.is_empty() {
                    self.res = bs.to_vec();
                } else {
                    ensure!(
                        self.res.len() == bs.len(),
                        "operands of 'bit_xor' must have the same lengths, got {:x?} and {:x?}",
                        self.res,
                        bs
                    );
                    for (l, r) in self.res.iter_mut().zip(bs.iter()) {
                        *l ^= *r;
                    }
                }
                Ok(())
            }
            v => bail!("cannot apply 'bit_xor' to {:?}", v),
        }
    }

    fn get(&self) -> Result<DataValue> {
        Ok(DataValue::Bytes(self.res.clone()))
    }
}

pub(crate) fn parse_aggr(name: &str) -> Option<&'static Aggregation> {
    Some(match name {
        "and" => &AGGR_AND,
        "or" => &AGGR_OR,
        "unique" => &AGGR_UNIQUE,
        "group_count" => &AGGR_GROUP_COUNT,
        "union" => &AGGR_UNION,
        "intersection" => &AGGR_INTERSECTION,
        "count" => &AGGR_COUNT,
        "count_unique" => &AGGR_COUNT_UNIQUE,
        "variance" => &AGGR_VARIANCE,
        "std_dev" => &AGGR_STD_DEV,
        "sum" => &AGGR_SUM,
        "product" => &AGGR_PRODUCT,
        "min" => &AGGR_MIN,
        "max" => &AGGR_MAX,
        "mean" => &AGGR_MEAN,
        "choice" => &AGGR_CHOICE,
        "collect" => &AGGR_COLLECT,
        "interval_coalesce" => &AGGR_INTERVAL_COALESCE,
        "shortest" => &AGGR_SHORTEST,
        "min_cost" => &AGGR_MIN_COST,
        "bit_and" => &AGGR_BIT_AND,
        "bit_or" => &AGGR_BIT_OR,
        "bit_xor" => &AGGR_BIT_XOR,
        "latest_by" => &AGGR_LATEST_BY,
        "smallest_by" => &AGGR_SMALLEST_BY,
        "choice_rand" => &AGGR_CHOICE_RAND,
        "min_cost_k" => &AGGR_MIN_COST_K,
        "pareto_min" => &AGGR_PARETO_MIN,
        "pareto_max" => &AGGR_PARETO_MAX,
        _ => return None,
    })
}

impl Aggregation {
    /// Build the candidate contract of a bounded-meet aggregate (provenance
    /// semirings R1), returning `(k, op)`. The trailing argument is the
    /// bound k — a positive integer.
    pub(crate) fn bounded_meet_init(
        &self,
        args: &[DataValue],
    ) -> Result<(usize, Box<dyn BoundedMeetAggrObj>)> {
        debug_assert!(self.is_bounded_meet);
        let k = match args {
            [k_arg] => k_arg.get_int().filter(|k| *k >= 1).ok_or_else(|| {
                miette!(
                    "the bound of '{}' must be a positive integer, got {:?}",
                    self.name,
                    k_arg
                )
            })?,
            _ => bail!(
                "'{}' takes exactly one argument after the value: the bound k",
                self.name
            ),
        };
        let op: Box<dyn BoundedMeetAggrObj> = match self.name {
            name if name == AGGR_MIN_COST_K.name => Box::new(BoundedMinCostK),
            name => unreachable!("{}", name),
        };
        Ok((k as usize, op))
    }
    pub(crate) fn meet_init(&mut self, _args: &[DataValue]) -> Result<()> {
        if let Some(f) = &self.meet_factory {
            if !_args.is_empty() {
                bail!("custom aggregate {} takes no arguments", self.name);
            }
            self.meet_op = Some(f());
            return Ok(());
        }
        self.meet_op.replace(match self.name {
            name if name == AGGR_AND.name => Box::new(MeetAggrAnd),
            name if name == AGGR_OR.name => Box::new(MeetAggrOr),
            name if name == AGGR_MIN.name => Box::new(MeetAggrMin),
            name if name == AGGR_MAX.name => Box::new(MeetAggrMax),
            name if name == AGGR_CHOICE.name => Box::new(MeetAggrChoice),
            name if name == AGGR_BIT_AND.name => Box::new(MeetAggrBitAnd),
            name if name == AGGR_BIT_OR.name => Box::new(MeetAggrBitOr),
            name if name == AGGR_UNION.name => Box::new(MeetAggrUnion),
            name if name == AGGR_INTERSECTION.name => Box::new(MeetAggrIntersection),
            name if name == AGGR_SHORTEST.name => Box::new(MeetAggrShortest),
            name if name == AGGR_MIN_COST.name => Box::new(MeetAggrMinCost),
            name => unreachable!("{}", name),
        });
        Ok(())
    }
    pub(crate) fn normal_init(&mut self, args: &[DataValue]) -> Result<()> {
        if let Some(f) = &self.meet_factory {
            if !args.is_empty() {
                bail!("custom aggregate {} takes no arguments", self.name);
            }
            let op = f();
            let state = op.init_val();
            self.normal_op = Some(Box::new(MeetToNormalAdapter { op, state }));
            return Ok(());
        }
        #[allow(clippy::box_default)]
        self.normal_op.replace(match self.name {
            name if name == AGGR_AND.name => Box::new(AggrAnd::default()),
            name if name == AGGR_OR.name => Box::new(AggrOr::default()),
            name if name == AGGR_COUNT.name => Box::new(AggrCount::default()),
            name if name == AGGR_GROUP_COUNT.name => Box::new(AggrGroupCount::default()),
            name if name == AGGR_COUNT_UNIQUE.name => Box::new(AggrCountUnique::default()),
            name if name == AGGR_SUM.name => Box::new(AggrSum::default()),
            name if name == AGGR_INT_SUM_PROD.name => Box::new(AggrIntSumProd::default()),
            name if name == AGGR_PRODUCT.name => Box::new(AggrProduct::default()),
            name if name == AGGR_MIN.name => Box::new(AggrMin::default()),
            name if name == AGGR_MAX.name => Box::new(AggrMax::default()),
            name if name == AGGR_MEAN.name => Box::new(AggrMean::default()),
            name if name == AGGR_VARIANCE.name => Box::new(AggrVariance::default()),
            name if name == AGGR_STD_DEV.name => Box::new(AggrStdDev::default()),
            name if name == AGGR_CHOICE.name => Box::new(AggrChoice::default()),
            name if name == AGGR_BIT_AND.name => Box::new(AggrBitAnd::default()),
            name if name == AGGR_BIT_OR.name => Box::new(AggrBitOr::default()),
            name if name == AGGR_BIT_XOR.name => Box::new(AggrBitXor::default()),
            name if name == AGGR_UNIQUE.name => Box::new(AggrUnique::default()),
            name if name == AGGR_UNION.name => Box::new(AggrUnion::default()),
            name if name == AGGR_INTERSECTION.name => Box::new(AggrIntersection::default()),
            name if name == AGGR_SHORTEST.name => Box::new(AggrShortest::default()),
            name if name == AGGR_MIN_COST.name => Box::new(AggrMinCost::default()),
            name if name == AGGR_LATEST_BY.name => Box::new(AggrLatestBy::default()),
            name if name == AGGR_SMALLEST_BY.name => Box::new(AggrSmallestBy::default()),
            name if name == AGGR_CHOICE_RAND.name => Box::new(AggrChoiceRand::default()),
            name if name == AGGR_INTERVAL_COALESCE.name => {
                Box::new(AggrIntervalCoalesce::default())
            }
            name if name == AGGR_COLLECT.name => Box::new({
                if args.is_empty() {
                    AggrCollect::default()
                } else {
                    let arg = args[0].get_int().ok_or_else(|| {
                        miette!(
                            "the argument to 'collect' must be an integer, got {:?}",
                            args[0]
                        )
                    })?;
                    ensure!(
                        arg > 0,
                        "argument to 'collect' must be positive, got {}",
                        arg
                    );
                    AggrCollect::new(arg as usize)
                }
            }),
            _ => unreachable!(),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(i: i64) -> DataValue {
        DataValue::from(i)
    }

    // The internal exact-i64 sum-of-products aggregate must accumulate exactly
    // and RETURN AN ERROR (never a wrapped, silently-wrong value) on i64
    // overflow — the correctness backbone of the factorized-count rewrite.
    #[test]
    fn int_sum_prod_is_exact_and_returns_int() {
        let mut a = AggrIntSumProd::default();
        a.set(&DataValue::List(vec![int(3), int(4)])).unwrap(); // 12
        a.set(&DataValue::List(vec![int(5), int(2)])).unwrap(); // +10
        a.set(&DataValue::List(vec![int(7)])).unwrap(); // +7
        assert_eq!(a.get().unwrap(), int(29));
    }

    #[test]
    fn int_sum_prod_empty_group_is_zero() {
        let a = AggrIntSumProd::default();
        assert_eq!(a.get().unwrap(), int(0));
    }

    #[test]
    fn int_sum_prod_product_overflow_errors_not_wraps() {
        let mut a = AggrIntSumProd::default();
        // 2^62 * 4 overflows i64 — must be a clean error, never a wrapped value.
        let r = a.set(&DataValue::List(vec![int(1i64 << 62), int(4)]));
        assert!(r.is_err(), "product overflow must error, got {:?}", r);
    }

    #[test]
    fn int_sum_prod_sum_overflow_errors_not_wraps() {
        let mut a = AggrIntSumProd::default();
        a.set(&DataValue::List(vec![int(i64::MAX)])).unwrap();
        let r = a.set(&DataValue::List(vec![int(1)]));
        assert!(r.is_err(), "sum overflow must error, got {:?}", r);
    }

    #[test]
    fn int_sum_prod_rejects_non_integer_factor() {
        let mut a = AggrIntSumProd::default();
        assert!(a
            .set(&DataValue::List(vec![int(3), DataValue::Null]))
            .is_err());
        assert!(a.set(&int(5)).is_err()); // not a list
    }
}
