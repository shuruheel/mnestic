/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 * Portions Copyright 2022, The Cozo Project Authors (the gate machinery
 * follows `bfs.rs`; the weighted-CSR surface follows `shortest_path_dijkstra.rs`).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `BudgetedTraversal` — multi-seed, cheapest-first, weighted expansion under a
//! required global distinct-node budget (spec: `docs/specs/budgeted-traversal.md`).
//!
//! ```text
//! ?[node, cost, parent, depth] <~ BudgetedTraversal(
//!     *edges[from, to, weight],      // or graph: 'projection' (edge slot drops out)
//!     *seeds[node, initial_cost?],   // required; column 1 optional, extra columns ignored
//!     *gate[node, ...],              // optional liveness gate (first column = node id)
//!     max_nodes: 200,                // required, positive integer: the distinct-node budget
//!     max_cost: 4.5,                 // optional, finite float >= 0: path-cost ceiling
//!     max_depth: 3,                  // optional, positive integer: exact hop bound (layered mode)
//!     undirected: false,             // optional, default false
//!     admit: cond,                   // optional predicate over the gate's columns; requires the gate
//! )
//! ```
//!
//! One output row per admitted node: `[node, cost, parent, depth]`, where
//! `parent`/`depth` witness one cheapest path (roots emit `parent = null`,
//! `depth = 0`). Termination: budget filled, or the reachable region exhausted.
//!
//! Determinism contract (spec §3.3): output is a pure function of the input
//! *sets* — never of tuple order, CSR neighbor order, or vertex-interning
//! order. Frontier ties break on `(cost, node value, hops)`; equal-cost
//! witnesses resolve to the lexicographic minimum `(cost, hops, parent value)`
//! among explored arrivals. Node identity is structural throughout (the engine
//! seam: `1` and `1.0` are *different* nodes in the id maps and gate probes,
//! while `==` inside `admit:` compares numerically).
//!
//! Costs: edge weights are consumed as given — the rule sums them, it does not
//! transform them. Callers wanting multiplicative confidences must store or
//! project `-ln(confidence)` themselves. Weights must be finite and
//! non-negative (missing weight column means every edge costs `1.0`).
//!
//! The gate relation is probed by *key prefix* on its first column; with
//! `admit:`, the node is admissible iff **some** gate row for it satisfies the
//! predicate; without `admit:`, bare row-presence admits. An inadmissible or
//! gate-absent node spends no budget and never relays expansion (it cannot be
//! a bridge). Footgun: `admit:` bindings resolve *positionally* against the
//! gate relation's own column list, so a binding name that does not match any
//! gate column silently never binds — keep gate column names and the `admit:`
//! expression aligned.
//!
//! `max_depth` switches the engine to layered per-`(node, hops)` labels so the
//! depth bound is *exact* on weighted graphs (a depth-pruned single-label
//! Dijkstra would miss cheaper-but-deeper detours; spec §8). Without
//! `max_depth`, plain single-label Dijkstra runs. `initial_cost` and
//! `max_cost` canonicalize `-0.0` to `+0.0` (one `total_cmp` convention
//! throughout).

use std::cmp::{Ordering, Reverse};
use std::collections::{btree_map::Entry, BTreeMap, BinaryHeap};

use graph::prelude::{DirectedCsrGraph, DirectedNeighborsWithValues};
use miette::{bail, ensure, Diagnostic, Result};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::expr::{eval_bytecode_pred, Bytecode, Expr};
use crate::data::program::WrongFixedRuleOptionError;
use crate::data::symb::Symbol;
use crate::data::tuple::Tuple;
use crate::data::value::DataValue;
use crate::fixed_rule::{FixedRule, FixedRuleInputRelation, FixedRulePayload};
use crate::parse::SourceSpan;
use crate::runtime::db::Poison;
use crate::runtime::graph_projection::VariantSpec;
use crate::runtime::temp_store::RegularTempStore;

pub(crate) struct BudgetedTraversal;

/// Batched cadence for `Poison` checks over pops + relaxations, the same
/// power-of-two mask the CSR build and the RA pipeline use.
const POISON_MASK: usize = 4096 - 1;

impl FixedRule for BudgetedTraversal {
    fn run(
        &self,
        payload: FixedRulePayload<'_, '_>,
        out: &mut RegularTempStore,
        poison: Poison,
    ) -> Result<()> {
        // All options are parsed before the CSR build: option errors are cheap
        // and must not cost a graph scan. "Unset" for the optional options is
        // detected with the `option_span` presence probe, never a sentinel
        // default — `max_depth` absent selects single-label mode outright, and
        // `max_cost` absent means unbounded (an INFINITY default would bypass
        // the finite-only validation below and give "unbounded" two spellings).
        let undirected = payload.bool_option("undirected", Some(false))?;
        let max_nodes = payload.pos_integer_option("max_nodes", None)?;
        let max_cost = if payload.option_span("max_cost").is_ok() {
            let mc = payload.float_option("max_cost", None)?;
            // NaN and ±inf are loud errors (spec §3.1, amendment A3): a NaN
            // ceiling would silently never prune, and "unbounded" is spelled
            // exactly one way — omit the option. `mc >= 0.` alone already
            // excludes NaN; `is_finite` adds the ±inf rejection.
            ensure!(
                mc.is_finite() && mc >= 0.,
                WrongFixedRuleOptionError {
                    name: "max_cost".to_string(),
                    span: payload.option_span("max_cost")?,
                    rule_name: payload.name().to_string(),
                    help: "a finite number >= 0 is required".to_string(),
                }
            );
            Some(if mc == 0. { 0. } else { mc })
        } else {
            None
        };
        let max_depth = if payload.option_span("max_depth").is_ok() {
            Some(payload.pos_integer_option("max_depth", None)?)
        } else {
            None
        };
        let admit_expr = if payload.option_span("admit").is_ok() {
            Some(payload.expr_option("admit", None)?)
        } else {
            None
        };

        let (source, input_base) =
            payload.graph_input(0, VariantSpec::weighted(undirected, true), &poison)?;
        let graph = source.weighted()?;
        let indices = source.indices();
        let inv_indices = source.inv_indices();

        let seeds = payload.get_input(input_base)?.ensure_min_len(1)?;
        let seed_costs = parse_seeds(&seeds)?;

        // An absent optional input still reads as `fixed_rule::not_enough_args`
        // after the projection shift, so presence is the bound Result itself.
        let gate_rel = payload.get_input(input_base + 1);
        if admit_expr.is_some() && gate_rel.is_err() {
            bail!(AdmitWithoutGateError(payload.option_span("admit")?));
        }
        let gate = match gate_rel {
            Err(_) => Gate {
                rel: None,
                pred: None,
                cache: Default::default(),
                stack: vec![],
            },
            Ok(g) => {
                // Arity >= the binding count (spec §3.1): an over-bound gate
                // would otherwise surface mid-traversal as eval's "tuple too
                // short — definitely a bug" when `admit:` reads the overflow
                // binding, or run silently when it does not. Reject it here,
                // at input validation, with the caller-facing arity error.
                let g = g.ensure_min_len(g.get_binding_map(0).len().max(1))?;
                let pred = match admit_expr {
                    None => None,
                    Some(mut e) => {
                        e.fill_binding_indices(&g.get_binding_map(0))?;
                        let bytecode = e.compile()?;
                        Some((bytecode, e.span()))
                    }
                };
                Gate {
                    rel: Some(g),
                    pred,
                    cache: Default::default(),
                    stack: vec![],
                }
            }
        };

        let labels = match max_depth {
            None => Labels::Single(Default::default()),
            Some(_) => Labels::Layered(Default::default()),
        };

        traverse(
            graph,
            indices,
            inv_indices,
            seed_costs,
            gate,
            labels,
            max_nodes,
            max_cost,
            max_depth,
            out,
            poison,
        )
    }

    fn arity(
        &self,
        _options: &BTreeMap<SmartString<LazyCompact>, Expr>,
        _rule_head: &[Symbol],
        _span: SourceSpan,
    ) -> Result<usize> {
        Ok(4)
    }

    fn supports_projection(&self) -> bool {
        true
    }
}

/// Parse the seeds relation into `node -> initial_cost`, min-merging
/// duplicates. Column 1 (`initial_cost`) is optional and must be a finite
/// number >= 0 when present; columns 2+ are dropped unread. `-0.0`
/// canonicalizes to `+0.0` so every cost in the run is `total_cmp`-clean.
fn parse_seeds(seeds: &FixedRuleInputRelation<'_, '_>) -> Result<BTreeMap<DataValue, f64>> {
    let mut map: BTreeMap<DataValue, f64> = BTreeMap::new();
    for tuple in seeds.iter()? {
        let tuple = tuple?;
        let mut it = tuple.into_iter();
        let node = it.next().unwrap(); // ensure_min_len(1) guarantees column 0
        let ic = match it.next() {
            None => 0.,
            Some(dv) => match dv.get_float() {
                Some(f) if f.is_finite() && f >= 0. => {
                    if f == 0. {
                        0.
                    } else {
                        f
                    }
                }
                _ => bail!(BadSeedCostError {
                    value: dv,
                    span: seeds.span()
                }),
            },
        };
        match map.entry(node) {
            Entry::Vacant(e) => {
                e.insert(ic);
            }
            Entry::Occupied(mut e) => {
                if ic < *e.get() {
                    *e.get_mut() = ic;
                }
            }
        }
    }
    Ok(map)
}

/// The in-expansion liveness gate. `rel: None` admits everything; with a gate
/// relation, admissibility is *existential* over the node's gate rows (spec
/// §3.1, amendment A4): with `admit:`, some row must satisfy the predicate;
/// without, bare row-presence admits. Verdicts are memoized both ways, so each
/// node is probed at most once per run, seeds included.
struct Gate<'a, 'b> {
    rel: Option<FixedRuleInputRelation<'a, 'b>>,
    pred: Option<(Vec<Bytecode>, SourceSpan)>,
    cache: BTreeMap<DataValue, bool>,
    stack: Vec<DataValue>,
}

impl Gate<'_, '_> {
    fn admissible(&mut self, dv: &DataValue) -> Result<bool> {
        let rel = match &self.rel {
            None => return Ok(true),
            Some(r) => *r,
        };
        if let Some(&b) = self.cache.get(dv) {
            return Ok(b);
        }
        let mut ok = false;
        for row in rel.prefix_iter(dv)? {
            let row = row?;
            if self.eval_admit(&row)? {
                ok = true;
                break;
            }
        }
        self.cache.insert(dv.clone(), ok);
        Ok(ok)
    }

    fn eval_admit(&mut self, row: &Tuple) -> Result<bool> {
        match &self.pred {
            None => Ok(true), // bare presence: any row admits
            Some((bytecode, span)) => eval_bytecode_pred(bytecode, row, &mut self.stack, *span),
        }
    }
}

/// A frontier entry is a `(cost, node value, hops)` *key* — improvements push
/// new entries and superseded ones are discarded at pop against the stored
/// label (lazy deletion). The node is compared as its `DataValue`, never as a
/// CSR id: interning order is input-form-dependent, and tie-breaks must not be
/// (spec §3.3). `slot` is payload only.
struct HeapEntry {
    cost: f64,
    node_dv: DataValue,
    hops: usize,
    slot: NodeSlot,
}

#[derive(Copy, Clone)]
enum NodeSlot {
    Csr(u32),
    /// A seed absent from the CSR id maps: it emits as a root and never
    /// expands. Pushed exactly once, so it needs no label entry.
    Loose,
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.cost
            .total_cmp(&other.cost)
            .then_with(|| self.node_dv.cmp(&other.node_dv))
            .then_with(|| self.hops.cmp(&other.hops))
    }
}
impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
// Not derived: the derive would compare the payload `slot`.
impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for HeapEntry {}

struct SingleLabel {
    cost: f64,
    hops: usize,
    parent: DataValue,
    settled: bool,
}

struct StateLabel {
    cost: f64,
    parent: DataValue,
    settled: bool,
}

struct NodeStates {
    /// Whether the node has emitted (first settled state). Later states of the
    /// node still settle and expand, but never re-emit or re-count.
    admitted: bool,
    /// Per-hops states, a strict `(cost, hops)` Pareto antichain: hops
    /// ascending implies cost strictly descending.
    states: BTreeMap<usize, StateLabel>,
}

/// Label store: one label per node without `max_depth`, per-`(node, hops)`
/// states with it (spec §8 — the exact-depth discipline). Keyed by CSR id,
/// which is safe *here*: the maps are only probed, never iterated for output,
/// so no interning-order dependence can leak.
enum Labels {
    Single(BTreeMap<u32, SingleLabel>),
    Layered(BTreeMap<u32, NodeStates>),
}

enum Settle {
    /// Superseded or dominance-removed: discard the pop.
    Stale,
    /// Mark settled; emit iff `first_for_node`. The witness fields come from
    /// the STORED label, never the popped key — equal-cost witness
    /// improvements that arrived after the entry was pushed are honored.
    Fresh {
        first_for_node: bool,
        cost: f64,
        hops: usize,
        parent: DataValue,
    },
}

impl Labels {
    fn insert_root(&mut self, id: u32, ic: f64) {
        match self {
            Labels::Single(map) => {
                map.insert(
                    id,
                    SingleLabel {
                        cost: ic,
                        hops: 0,
                        parent: DataValue::Null,
                        settled: false,
                    },
                );
            }
            Labels::Layered(map) => {
                map.insert(
                    id,
                    NodeStates {
                        admitted: false,
                        states: BTreeMap::from([(
                            0,
                            StateLabel {
                                cost: ic,
                                parent: DataValue::Null,
                                settled: false,
                            },
                        )]),
                    },
                );
            }
        }
    }

    /// Record an arrival at `u` with `(cost c, hops h, parent)`. Returns
    /// whether to push a heap entry: only on vacancy or *cost-strict*
    /// improvement — an equal-cost witness-only improvement reuses the
    /// already-queued `(cost, node, hops)` key, and settle-from-stored makes
    /// that entry deliver the improved witness.
    fn try_relax(&mut self, u: u32, c: f64, h: usize, parent: &DataValue) -> bool {
        match self {
            Labels::Single(map) => match map.entry(u) {
                Entry::Vacant(e) => {
                    e.insert(SingleLabel {
                        cost: c,
                        hops: h,
                        parent: parent.clone(),
                        settled: false,
                    });
                    true
                }
                Entry::Occupied(mut e) => {
                    let l = e.get_mut();
                    // Post-settle arrivals are never adopted: the row is
                    // already emitted, and with non-negative weights only
                    // witness fields could improve (spec §3.3).
                    if l.settled {
                        return false;
                    }
                    let ord = c
                        .total_cmp(&l.cost)
                        .then_with(|| h.cmp(&l.hops))
                        .then_with(|| parent.cmp(&l.parent));
                    if ord != Ordering::Less {
                        return false;
                    }
                    let cost_improved = c.total_cmp(&l.cost) == Ordering::Less;
                    l.cost = c;
                    l.hops = h;
                    l.parent = parent.clone();
                    cost_improved
                }
            },
            Labels::Layered(map) => {
                let ns = map.entry(u).or_insert_with(|| NodeStates {
                    admitted: false,
                    states: BTreeMap::new(),
                });
                // Dominance-in (strict (cost, hops) Pareto): drop the newcomer
                // iff some state with fewer hops costs no more. By the
                // antichain invariant the fewest-hops-below neighbor is the
                // cheapest below, so one `next_back` suffices.
                if let Some((_, below)) = ns.states.range(..h).next_back() {
                    if below.cost.total_cmp(&c) != Ordering::Greater {
                        return false;
                    }
                }
                // Within-state: strict lexicographic (cost, parent)
                // improvement — never keep-first (a kept first arrival would
                // make `parent` CSR-neighbor-order-dependent).
                let push = match ns.states.entry(h) {
                    Entry::Vacant(e) => {
                        e.insert(StateLabel {
                            cost: c,
                            parent: parent.clone(),
                            settled: false,
                        });
                        true
                    }
                    Entry::Occupied(mut e) => {
                        let s = e.get_mut();
                        if s.settled {
                            return false;
                        }
                        let ord = c.total_cmp(&s.cost).then_with(|| parent.cmp(&s.parent));
                        if ord != Ordering::Less {
                            return false;
                        }
                        let cost_improved = c.total_cmp(&s.cost) == Ordering::Less;
                        s.cost = c;
                        s.parent = parent.clone();
                        cost_improved
                    }
                };
                // Dominance-out: the newcomer removes every deeper state that
                // costs at least as much (settled or not — a pop that finds
                // its state missing reads as stale). By the antichain
                // invariant the doomed states are a contiguous run upward.
                let doomed: Vec<usize> = ns
                    .states
                    .range(h + 1..)
                    .take_while(|(_, s)| s.cost.total_cmp(&c) != Ordering::Less)
                    .map(|(&h2, _)| h2)
                    .collect();
                for h2 in doomed {
                    ns.states.remove(&h2);
                }
                push
            }
        }
    }

    fn settle(&mut self, v: u32, popped_cost: f64, popped_hops: usize) -> Settle {
        match self {
            Labels::Single(map) => {
                let Some(l) = map.get_mut(&v) else {
                    return Settle::Stale;
                };
                // A mismatched cost means the label improved after this entry
                // was pushed and re-queued its new cost: superseded.
                if l.settled || l.cost.total_cmp(&popped_cost) != Ordering::Equal {
                    return Settle::Stale;
                }
                l.settled = true;
                Settle::Fresh {
                    first_for_node: true,
                    cost: l.cost,
                    hops: l.hops,
                    parent: l.parent.clone(),
                }
            }
            Labels::Layered(map) => {
                let Some(ns) = map.get_mut(&v) else {
                    return Settle::Stale;
                };
                let Some(s) = ns.states.get_mut(&popped_hops) else {
                    return Settle::Stale; // dominance-removed
                };
                if s.settled || s.cost.total_cmp(&popped_cost) != Ordering::Equal {
                    return Settle::Stale;
                }
                s.settled = true;
                let first_for_node = !ns.admitted;
                ns.admitted = true;
                Settle::Fresh {
                    first_for_node,
                    cost: s.cost,
                    hops: popped_hops,
                    parent: s.parent.clone(),
                }
            }
        }
    }
}

/// The pop / settle / emit / expand loop. There is deliberately no
/// empty-graph early return: with an empty edge input every gated seed
/// classifies as loose at push time, emits `(seed, cost, null, 0)`, and never
/// expands — the CSR is only consulted for `NodeSlot::Csr` entries.
#[allow(clippy::too_many_arguments)]
fn traverse(
    graph: &DirectedCsrGraph<u32, (), f32>,
    indices: &[DataValue],
    inv_indices: &BTreeMap<DataValue, u32>,
    seed_costs: BTreeMap<DataValue, f64>,
    mut gate: Gate<'_, '_>,
    mut labels: Labels,
    max_nodes: usize,
    max_cost: Option<f64>,
    max_depth: Option<usize>,
    out: &mut RegularTempStore,
    poison: Poison,
) -> Result<()> {
    let mut frontier: BinaryHeap<Reverse<HeapEntry>> = BinaryHeap::new();
    let mut admitted = 0usize;
    let mut ops = 0usize; // one counter over pops + relaxations

    for (dv, ic) in seed_costs {
        // An over-ceiling or gated-out seed spends nothing: no budget, no
        // label, no heap entry, no emission. Ceiling before gate — both are
        // pure prunes, and the ceiling is cheaper.
        if let Some(mc) = max_cost {
            if ic > mc {
                continue;
            }
        }
        if !gate.admissible(&dv)? {
            continue;
        }
        match inv_indices.get(&dv) {
            Some(&id) => {
                // Root label (cost, hops 0, parent null): the §5 root-tie rule
                // needs no code — hops 0 and the Null parent make it the
                // lexicographic minimum among equal-cost labels.
                labels.insert_root(id, ic);
                frontier.push(Reverse(HeapEntry {
                    cost: ic,
                    node_dv: dv,
                    hops: 0,
                    slot: NodeSlot::Csr(id),
                }));
            }
            None => frontier.push(Reverse(HeapEntry {
                cost: ic,
                node_dv: dv,
                hops: 0,
                slot: NodeSlot::Loose,
            })),
        }
    }

    'main: while let Some(Reverse(entry)) = frontier.pop() {
        ops += 1;
        if ops & POISON_MASK == 0 {
            poison.check()?;
        }

        let (first, cost, hops, parent) = match entry.slot {
            NodeSlot::Loose => (true, entry.cost, 0usize, DataValue::Null),
            NodeSlot::Csr(v) => match labels.settle(v, entry.cost, entry.hops) {
                Settle::Stale => continue 'main,
                Settle::Fresh {
                    first_for_node,
                    cost,
                    hops,
                    parent,
                } => (first_for_node, cost, hops, parent),
            },
        };

        if first {
            out.put(vec![
                entry.node_dv.clone(),
                DataValue::from(cost),
                parent,
                DataValue::from(hops as i64),
            ]);
            admitted += 1;
            // Budget stop before expanding: the last admitted node's
            // expansion cannot admit anyone (spec §3.2).
            if admitted == max_nodes {
                break 'main;
            }
        }

        let NodeSlot::Csr(v) = entry.slot else {
            continue 'main;
        };
        for t in graph.out_neighbors_with_values(v) {
            ops += 1;
            if ops & POISON_MASK == 0 {
                poison.check()?;
            }
            let c2 = cost + t.value as f64;
            let h2 = hops + 1;
            if let Some(mc) = max_cost {
                if c2 > mc {
                    continue;
                }
            }
            if let Some(md) = max_depth {
                if h2 > md {
                    continue;
                }
            }
            let u_dv = &indices[t.target as usize];
            // Gate before any label/heap touch: an inadmissible node spends
            // zero budget and is never a bridge.
            if !gate.admissible(u_dv)? {
                continue;
            }
            if labels.try_relax(t.target, c2, h2, &entry.node_dv) {
                frontier.push(Reverse(HeapEntry {
                    cost: c2,
                    node_dv: u_dv.clone(),
                    hops: h2,
                    slot: NodeSlot::Csr(t.target),
                }));
            }
        }
    }
    Ok(())
}

#[derive(Error, Diagnostic, Debug)]
#[error("Seed initial cost {value:?} is invalid")]
#[diagnostic(code(algo::bad_seed_cost))]
#[diagnostic(help(
    "The optional `initial_cost` column (seeds column 1) must be a finite number >= 0"
))]
struct BadSeedCostError {
    value: DataValue,
    #[label]
    span: SourceSpan,
}

#[derive(Error, Diagnostic, Debug)]
#[error("Option 'admit' was given but no gate relation input is present")]
#[diagnostic(code(algo::admit_without_gate))]
#[diagnostic(help(
    "Pass a gate relation as the trailing input (first column = node id) or remove 'admit'"
))]
struct AdmitWithoutGateError(#[label] SourceSpan);
