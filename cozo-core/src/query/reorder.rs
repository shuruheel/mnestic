/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::collections::{BTreeMap, BTreeSet};
use std::mem;

use miette::{bail, Diagnostic, Result};
use thiserror::Error;

use crate::data::expr::Expr;
use crate::data::program::{NormalFormAtom, NormalFormInlineRule, ReorderMode, Unification};
use crate::data::symb::Symbol;
use crate::data::value::DataValue;
use crate::parse::SourceSpan;
use crate::runtime::transact::SessionTx;

#[derive(Diagnostic, Debug, Error)]
#[error("Encountered unsafe negation, or empty rule definition")]
#[diagnostic(code(eval::unsafe_negation))]
#[diagnostic(help(
    "Only rule applications that are partially bounded, \
or expressions / unifications that are completely bounded, can be safely negated. \
You may also encounter this error if your rule can never produce any rows."
))]
pub(crate) struct UnsafeNegation(#[label] pub(crate) SourceSpan);

#[derive(Diagnostic, Debug, Error)]
#[error("Atom contains unbound variable, or rule contains no variable at all")]
#[diagnostic(code(eval::unbound_variable))]
pub(crate) struct UnboundVariable(#[label] pub(crate) SourceSpan);

/// mnestic fork (#1): predicate-pushdown for equality post-filters on stored
/// relations.
///
/// Upstream Cozo compiles `*rel[k, ..], k == <ground>` to a full `load_stored`
/// scan with an `eq(..)` post-filter, even when `k` is a key column — whereas the
/// semantically identical binding-first form `k = <ground>, *rel{k, ..}` compiles
/// to a keyed `stored_prefix_join`. This pass rewrites the former into the latter:
/// it converts a qualifying `Predicate(eq(var, ground))` into a `Unification` and
/// hoists *only those converted unifications* to the front, so the relation atom
/// that produces `var` binds it as a join key and the existing well-ordering logic
/// below emits a prefix lookup.
///
/// Deliberately conservative:
/// - Only `Predicate` atoms are touched; user-written unifications and every other
///   atom keep their exact original relative order, so no existing query's behavior
///   changes — we only rewrite the `==`-post-filter shape.
/// - Numeric ground values are NOT converted (see `eq_predicate_as_unification`):
///   `op_eq` treats `Int(n) == Float(n)` as equal across types, but a keyed lookup
///   uses the index's strict `Num` ordering where `Int(n) != Float(n)`, so
///   converting a numeric equality could silently drop cross-type matches.
///
/// Result sets are therefore unchanged; this is purely an optimization.
fn push_equality_filters_to_bindings(body: Vec<NormalFormAtom>) -> Vec<NormalFormAtom> {
    // Variables produced by positive stored-relation atoms. Only equalities on
    // these are converted: converting an equality on a variable that no atom
    // generates would turn a (possibly erroneous) filter into a generator and
    // silence a genuine unbound-variable error.
    let mut generated: BTreeSet<Symbol> = BTreeSet::new();
    for atom in &body {
        if let NormalFormAtom::Relation(r) = atom {
            generated.extend(r.args.iter().cloned());
        }
    }

    // Converted equality bindings are hoisted to the front (preserving their
    // relative order); everything else keeps its original relative order.
    let mut front = vec![];
    let mut rest = vec![];
    for atom in body {
        match atom {
            NormalFormAtom::Predicate(expr) => match eq_predicate_as_unification(&expr, &generated)
            {
                Some(unif) => front.push(NormalFormAtom::Unification(unif)),
                None => rest.push(NormalFormAtom::Predicate(expr)),
            },
            other => rest.push(other),
        }
    }
    front.extend(rest);
    front
}

/// If `expr` is `eq(v, g)` or `eq(g, v)` where `v` is a bare variable in
/// `generated` and `g` is a NON-numeric ground value, returns the equivalent
/// `v = g` unification. Otherwise `None`.
fn eq_predicate_as_unification(expr: &Expr, generated: &BTreeSet<Symbol>) -> Option<Unification> {
    let (op, args, span) = match expr {
        Expr::Apply { op, args, span } => (op, args, *span),
        _ => return None,
    };
    if op.name != "OP_EQ" || args.len() != 2 {
        return None;
    }
    for (maybe_var, maybe_ground) in [(&args[0], &args[1]), (&args[1], &args[0])] {
        if let Expr::Binding { var, .. } = maybe_var {
            // The other side must have no free variables.
            let ground = maybe_ground
                .bindings()
                .map(|b| b.is_empty())
                .unwrap_or(false);
            // Refuse NUMERIC grounds: `op_eq` treats `Int(n) == Float(n)` as equal
            // (cross-type), but a keyed prefix lookup uses the key index's strict
            // `Num` ordering where `Int(n) != Float(n)`. Converting a numeric
            // equality would therefore silently drop cross-type matches. Non-numeric
            // values (str/uuid/bytes/bool/null) compare identically under `op_eq` and
            // the index. Parameters are already substituted to `Expr::Const` by this
            // stage, so this also covers `k == $numeric_param`.
            let numeric_const = matches!(
                maybe_ground,
                Expr::Const {
                    val: DataValue::Num(_),
                    ..
                }
            );
            if ground && !numeric_const && generated.contains(var) {
                return Some(Unification {
                    binding: var.clone(),
                    expr: maybe_ground.clone(),
                    one_many_unif: false,
                    span,
                });
            }
        }
    }
    None
}

impl NormalFormInlineRule {
    pub(crate) fn convert_to_well_ordered_rule(
        self,
        tx: &SessionTx<'_>,
        reorder_mode: ReorderMode,
        rule_name: &str,
        key_arity_cache: &mut BTreeMap<Symbol, Option<usize>>,
    ) -> Result<Self> {
        let NormalFormInlineRule { head, aggr, body } = self;

        // mnestic #1: rewrite equality post-filters on stored relations into
        // hoisted bindings so keyed lookups compile to prefix joins.
        let body = push_equality_filters_to_bindings(body);

        // mnestic (join-reorder, 0.10.5): deterministic, stat-free greedy join
        // reorder. Runs immediately after the #1 equality-pushdown, before the
        // binding-before-use well-ordering below. Default ON (`:reorder greedy`);
        // `:reorder written` (or a `:limit` without `:sort`) leaves the written
        // order untouched. See `greedy_reorder_conjunction`.
        match reorder_mode {
            ReorderMode::Written => {
                let body = well_order_body(body)?;
                Ok(NormalFormInlineRule { head, aggr, body })
            }
            ReorderMode::Greedy => {
                match greedy_reorder_conjunction(&body, tx, rule_name, key_arity_cache) {
                    // Ineligible or identity permutation: use the written order.
                    // The identity fast path keeps hand-tuned (greedy-consistent)
                    // queries byte-identical.
                    None => {
                        let body = well_order_body(body)?;
                        Ok(NormalFormInlineRule { head, aggr, body })
                    }
                    Some(reordered) => match well_order_body(reordered) {
                        Ok(body) => Ok(NormalFormInlineRule { head, aggr, body }),
                        // Safety valve: the permuted body failed the well-ordering
                        // fixpoint (e.g. an unbindable pending-unification chain).
                        // Retry the original written order so the pass never
                        // introduces a NEW compile failure and error spans keep
                        // referencing the user's written text.
                        Err(_) => {
                            let body = well_order_body(body)?;
                            Ok(NormalFormInlineRule { head, aggr, body })
                        }
                    },
                }
            }
        }
    }
}

/// The upstream Cozo binding-before-use pass: floats predicates, unifications,
/// negations and search atoms to their earliest fully-bound position while
/// leaving positive Rule/Relation atoms in their given relative order. Positive
/// atom order is decided by the caller (written order, or the greedy reorder).
fn well_order_body(body: Vec<NormalFormAtom>) -> Result<Vec<NormalFormAtom>> {
    let mut seen_variables = BTreeSet::default();
    let mut round_1_collected = vec![];
    let mut pending = vec![];

    // first round: collect all unifications that are completely bounded
    for atom in body {
        match atom {
            NormalFormAtom::Unification(u) => {
                if u.is_const() {
                    seen_variables.insert(u.binding.clone());
                    round_1_collected.push(NormalFormAtom::Unification(u));
                } else {
                    let unif_vars = u.bindings_in_expr()?;
                    if unif_vars.is_subset(&seen_variables) {
                        seen_variables.insert(u.binding.clone());
                        round_1_collected.push(NormalFormAtom::Unification(u));
                    } else {
                        pending.push(NormalFormAtom::Unification(u));
                    }
                }
            }
            NormalFormAtom::Rule(mut r) => {
                for arg in &mut r.args {
                    seen_variables.insert(arg.clone());
                }
                round_1_collected.push(NormalFormAtom::Rule(r))
            }
            NormalFormAtom::Relation(v) => {
                for arg in &v.args {
                    seen_variables.insert(arg.clone());
                }
                round_1_collected.push(NormalFormAtom::Relation(v))
            }
            NormalFormAtom::NegatedRule(r) => pending.push(NormalFormAtom::NegatedRule(r)),
            NormalFormAtom::NegatedRelation(v) => {
                pending.push(NormalFormAtom::NegatedRelation(v))
            }
            NormalFormAtom::Predicate(p) => {
                pending.push(NormalFormAtom::Predicate(p));
            }
            NormalFormAtom::HnswSearch(s) => {
                if seen_variables.contains(&s.query) {
                    seen_variables.extend(s.all_bindings().cloned());
                    round_1_collected.push(NormalFormAtom::HnswSearch(s));
                } else {
                    pending.push(NormalFormAtom::HnswSearch(s));
                }
            }
            NormalFormAtom::FtsSearch(s) => {
                if seen_variables.contains(&s.query) {
                    seen_variables.extend(s.all_bindings().cloned());
                    round_1_collected.push(NormalFormAtom::FtsSearch(s));
                } else {
                    pending.push(NormalFormAtom::FtsSearch(s));
                }
            }
            NormalFormAtom::LshSearch(s) => {
                if seen_variables.contains(&s.query) {
                    seen_variables.extend(s.all_bindings().cloned());
                    round_1_collected.push(NormalFormAtom::LshSearch(s));
                } else {
                    pending.push(NormalFormAtom::LshSearch(s));
                }
            }
        }
    }

    let mut collected = vec![];
    seen_variables.clear();
    let mut last_pending = vec![];
    // second round: insert pending where possible
    for atom in round_1_collected {
        mem::swap(&mut last_pending, &mut pending);
        pending.clear();
        match atom {
            NormalFormAtom::Rule(r) => {
                seen_variables.extend(r.args.iter().cloned());
                collected.push(NormalFormAtom::Rule(r))
            }
            NormalFormAtom::Relation(v) => {
                seen_variables.extend(v.args.iter().cloned());
                collected.push(NormalFormAtom::Relation(v))
            }
            NormalFormAtom::NegatedRule(_)
            | NormalFormAtom::NegatedRelation(_)
            | NormalFormAtom::Predicate(_) => {
                unreachable!()
            }
            NormalFormAtom::Unification(u) => {
                seen_variables.insert(u.binding.clone());
                collected.push(NormalFormAtom::Unification(u));
            }
            NormalFormAtom::HnswSearch(s) => {
                seen_variables.extend(s.all_bindings().cloned());
                collected.push(NormalFormAtom::HnswSearch(s));
            }
            NormalFormAtom::FtsSearch(s) => {
                seen_variables.extend(s.all_bindings().cloned());
                collected.push(NormalFormAtom::FtsSearch(s));
            }
            NormalFormAtom::LshSearch(s) => {
                seen_variables.extend(s.all_bindings().cloned());
                collected.push(NormalFormAtom::LshSearch(s));
            }
        }
        for atom in last_pending.iter() {
            match atom {
                NormalFormAtom::Rule(_) | NormalFormAtom::Relation(_) => unreachable!(),
                NormalFormAtom::NegatedRule(r) => {
                    if r.args.iter().all(|a| seen_variables.contains(a)) {
                        collected.push(NormalFormAtom::NegatedRule(r.clone()));
                    } else {
                        pending.push(NormalFormAtom::NegatedRule(r.clone()));
                    }
                }
                NormalFormAtom::NegatedRelation(v) => {
                    if v.args.iter().all(|a| seen_variables.contains(a)) {
                        collected.push(NormalFormAtom::NegatedRelation(v.clone()));
                    } else {
                        pending.push(NormalFormAtom::NegatedRelation(v.clone()));
                    }
                }
                NormalFormAtom::HnswSearch(s) => {
                    if seen_variables.contains(&s.query) {
                        seen_variables.extend(s.all_bindings().cloned());
                        collected.push(NormalFormAtom::HnswSearch(s.clone()));
                    } else {
                        pending.push(NormalFormAtom::HnswSearch(s.clone()));
                    }
                }
                NormalFormAtom::FtsSearch(s) => {
                    if seen_variables.contains(&s.query) {
                        seen_variables.extend(s.all_bindings().cloned());
                        collected.push(NormalFormAtom::FtsSearch(s.clone()));
                    } else {
                        pending.push(NormalFormAtom::FtsSearch(s.clone()));
                    }
                }
                NormalFormAtom::LshSearch(s) => {
                    if seen_variables.contains(&s.query) {
                        seen_variables.extend(s.all_bindings().cloned());
                        collected.push(NormalFormAtom::LshSearch(s.clone()));
                    } else {
                        pending.push(NormalFormAtom::LshSearch(s.clone()));
                    }
                }
                NormalFormAtom::Predicate(p) => {
                    if p.bindings()?.is_subset(&seen_variables) {
                        collected.push(NormalFormAtom::Predicate(p.clone()));
                    } else {
                        pending.push(NormalFormAtom::Predicate(p.clone()));
                    }
                }
                NormalFormAtom::Unification(u) => {
                    if u.bindings_in_expr()?.is_subset(&seen_variables) {
                        collected.push(NormalFormAtom::Unification(u.clone()));
                    } else {
                        pending.push(NormalFormAtom::Unification(u.clone()));
                    }
                }
            }
        }
    }

    if !pending.is_empty() {
        for atom in pending {
            match atom {
                NormalFormAtom::Rule(_) | NormalFormAtom::Relation(_) => unreachable!(),
                NormalFormAtom::NegatedRule(r) => {
                    if r.args.iter().any(|a| seen_variables.contains(a)) {
                        collected.push(NormalFormAtom::NegatedRule(r.clone()));
                    } else {
                        bail!(UnsafeNegation(r.span));
                    }
                }
                NormalFormAtom::NegatedRelation(v) => {
                    if v.args.iter().any(|a| seen_variables.contains(a)) {
                        collected.push(NormalFormAtom::NegatedRelation(v.clone()));
                    } else {
                        bail!(UnsafeNegation(v.span));
                    }
                }
                NormalFormAtom::Predicate(p) => {
                    bail!(UnboundVariable(p.span()))
                }
                NormalFormAtom::Unification(u) => {
                    bail!(UnboundVariable(u.span))
                }
                NormalFormAtom::HnswSearch(s) => {
                    bail!(UnboundVariable(s.span))
                }
                NormalFormAtom::FtsSearch(s) => {
                    bail!(UnboundVariable(s.span))
                }
                NormalFormAtom::LshSearch(s) => {
                    bail!(UnboundVariable(s.span))
                }
            }
        }
    }

    Ok(collected)
}

/// Number of key (primary-key prefix) columns of a stored relation, memoized in
/// a per-normalization local cache.
///
/// `SessionTx` has no relation-handle cache (`get_relation` does a fresh storage
/// point-get + decode on every call), so we memoize here to keep the reorder
/// pass close to free even for rules that reference the same relation many
/// times. `None` = relation not found (compile will raise the proper error);
/// the greedy pass then treats its key arity as 0.
fn key_arity_of(
    tx: &SessionTx<'_>,
    name: &Symbol,
    cache: &mut BTreeMap<Symbol, Option<usize>>,
) -> Option<usize> {
    if let Some(v) = cache.get(name) {
        return *v;
    }
    let v = tx
        .get_relation(name, false)
        .ok()
        .map(|h| h.metadata.keys.len());
    cache.insert(name.clone(), v);
    v
}

/// A positive stored-relation atom prepared for greedy reordering.
struct RelCand {
    /// Position of this atom among the positive relation atoms in written order.
    orig_idx: usize,
    /// The original `NormalFormAtom::Relation(..)` (cloned for re-emission).
    atom: NormalFormAtom,
    /// Non-wildcard variables the atom binds (wildcards are unique `~` symbols
    /// and are never shared, so they are excluded from connectivity / new-var
    /// scoring).
    vars: BTreeSet<Symbol>,
    /// Positional arguments (used for the bound-key-prefix tie-break).
    args: Vec<Symbol>,
    /// Number of primary-key columns of the underlying relation.
    key_arity: usize,
}

/// Length of the bound prefix of `cand`'s primary key given the bound set `b`:
/// how many leading key columns (positions 0, 1, 2, …) are bound. A wildcard or
/// an unbound column breaks the prefix. A longer bound prefix means the atom
/// compiles to a keyed `stored_prefix_join` rather than a scan, so we prefer it.
fn bound_key_prefix_len(cand: &RelCand, b: &BTreeSet<Symbol>) -> usize {
    let mut n = 0;
    for arg in cand.args.iter().take(cand.key_arity) {
        if arg.is_generated_ignored_symbol() || !b.contains(arg) {
            break;
        }
        n += 1;
    }
    n
}

/// Deterministic, stat-free "min-new-vars" greedy join reorder (mnestic fork,
/// 0.10.5). An LLM authoring CozoScript will not hand-tune join order; a naive
/// members-first conjunction can spin on an N³ intermediate even though every
/// step is a connected prefix join. This pass repeatedly appends, among the
/// atoms connected to what is already bound, the one introducing the fewest new
/// variables (pulling 0-new-var atoms forward is semi-join filter pushdown),
/// converting that N³ blow-up to N². It changes no results: a conjunction of
/// generator atoms is commutative under set semantics, and the caller re-derives
/// binding-before-use afterwards.
///
/// Returns `None` when the rule is ineligible (see below) or when the greedy
/// order equals the written order (identity) — in both cases the caller keeps
/// the written body untouched, so greedy-consistent hand-tuned queries produce
/// byte-identical plans. Returns `Some(reordered_body)` otherwise.
///
/// # Eligibility (conservative v1 — all must hold)
/// - Every positive body atom is a stored `Relation` atom. Any derived-rule
///   application (`Rule`) makes the magic-sets/recursion interaction non-trivial,
///   and any `Hnsw`/`Fts`/`Lsh` search atom has fixed placement — either aborts.
/// - There are at least 3 positive relation atoms (fewer cannot exhibit the
///   blow-up and there is nothing to gain).
///
/// The `:limit`-without-`:sort` guard (row-subset stability under early return)
/// is enforced query-wide by the caller, which forces `ReorderMode::Written`.
fn greedy_reorder_conjunction(
    body: &[NormalFormAtom],
    tx: &SessionTx<'_>,
    rule_name: &str,
    key_arity_cache: &mut BTreeMap<Symbol, Option<usize>>,
) -> Option<Vec<NormalFormAtom>> {
    // --- Eligibility -------------------------------------------------------
    let mut relation_indices = vec![];
    for (i, atom) in body.iter().enumerate() {
        match atom {
            NormalFormAtom::Relation(_) => relation_indices.push(i),
            // Derived-rule applications and search atoms disqualify the whole
            // rule (keeps magic-sets/recursion interaction empty by construction
            // and never disturbs fixed-placement search atoms).
            NormalFormAtom::Rule(_)
            | NormalFormAtom::HnswSearch(_)
            | NormalFormAtom::FtsSearch(_)
            | NormalFormAtom::LshSearch(_) => return None,
            _ => {}
        }
    }
    if relation_indices.len() < 3 {
        return None;
    }
    let first_rel = relation_indices[0];

    // --- Seed the bound set ------------------------------------------------
    // Only unifications positioned BEFORE the first positive atom are hoisted
    // (the #1 pre-pass hoists its converted equalities to the front, and any
    // leading user unification stays in front). A const unification written
    // AFTER a relation is collected in place, so seeding from it would mis-score
    // the bound-key-prefix tie-break. Hence: seed from leading unifications only.
    let mut bound: BTreeSet<Symbol> = BTreeSet::new();
    for atom in &body[..first_rel] {
        if let NormalFormAtom::Unification(u) = atom {
            bound.insert(u.binding.clone());
        }
    }

    // --- Build candidate descriptors (written order) -----------------------
    let mut cands: Vec<RelCand> = Vec::with_capacity(relation_indices.len());
    for (ord, &bi) in relation_indices.iter().enumerate() {
        if let NormalFormAtom::Relation(r) = &body[bi] {
            let vars: BTreeSet<Symbol> = r
                .args
                .iter()
                .filter(|a| !a.is_generated_ignored_symbol())
                .cloned()
                .collect();
            let key_arity = key_arity_of(tx, &r.name, key_arity_cache).unwrap_or(0);
            cands.push(RelCand {
                orig_idx: ord,
                atom: body[bi].clone(),
                vars,
                args: r.args.clone(),
                key_arity,
            });
        }
    }

    // --- Greedy selection --------------------------------------------------
    let mut remaining: Vec<usize> = (0..cands.len()).collect();
    let mut order: Vec<usize> = Vec::with_capacity(cands.len());
    let mut b = bound;
    let mut has_cartesian_step = false;
    let mut first_pick = true;
    while !remaining.is_empty() {
        // Candidates sharing at least one non-wildcard variable with the bound
        // set (i.e. joinable without a Cartesian product).
        let connected: Vec<usize> = remaining
            .iter()
            .copied()
            .filter(|&ci| cands[ci].vars.iter().any(|v| b.contains(v)))
            .collect();
        let pick = if connected.is_empty() {
            // No connected candidate. The earliest-written remaining atom yields
            // the provably minimal number of Cartesian steps (= #components − 1
            // of the variable-sharing graph). The very first pick is the base
            // scan, not a Cartesian join.
            if !first_pick {
                has_cartesian_step = true;
            }
            *remaining
                .iter()
                .min_by_key(|&&ci| cands[ci].orig_idx)
                .unwrap()
        } else {
            // argmin over (new-vars ASC, bound-key-prefix DESC, written idx ASC).
            // orig_idx is unique, so the ordering is total and deterministic.
            *connected
                .iter()
                .min_by(|&&a, &&bx| {
                    let ca = &cands[a];
                    let cb = &cands[bx];
                    let na = ca.vars.iter().filter(|v| !b.contains(*v)).count();
                    let nb = cb.vars.iter().filter(|v| !b.contains(*v)).count();
                    na.cmp(&nb)
                        .then(bound_key_prefix_len(cb, &b).cmp(&bound_key_prefix_len(ca, &b)))
                        .then(ca.orig_idx.cmp(&cb.orig_idx))
                })
                .unwrap()
        };
        first_pick = false;
        for v in &cands[pick].vars {
            b.insert(v.clone());
        }
        order.push(pick);
        remaining.retain(|&ci| ci != pick);
    }

    // Diagnostic: even the greedy order still contains a Cartesian step. Warn so
    // agent frameworks can surface it; `::explain` also annotates the op.
    if has_cartesian_step {
        log::warn!(
            "join-reorder: rule `{rule_name}` still contains a Cartesian step \
             (a disconnected conjunction); consider revising the query"
        );
    }

    // --- Safety valve (a): identity permutation -> untouched ---------------
    if order.iter().enumerate().all(|(pos, &ci)| ci == pos) {
        return None;
    }

    // --- Reconstruct the body ----------------------------------------------
    // leading (pre-first-relation atoms, incl. hoisted #1 equalities) stay in
    // front; the relation atoms are re-emitted in greedy order; every remaining
    // non-relation atom follows (the well-ordering pass re-floats each to its
    // earliest fully-bound slot, so their trailing position is immaterial).
    let mut new_body: Vec<NormalFormAtom> = Vec::with_capacity(body.len());
    new_body.extend_from_slice(&body[..first_rel]);
    for &ci in &order {
        new_body.push(cands[ci].atom.clone());
    }
    for atom in &body[first_rel..] {
        if !matches!(atom, NormalFormAtom::Relation(_)) {
            new_body.push(atom.clone());
        }
    }
    Some(new_body)
}
