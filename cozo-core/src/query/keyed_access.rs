/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file, You can obtain one at
 * https://mozilla.org/MPL/2.0/.
 */

//! Local access bindings after ordering and magic rewriting. Original equalities
//! remain as residuals. This pass never replaces an existing binding producer.
use std::collections::{BTreeMap, BTreeSet};

use crate::data::expr::Expr;
use crate::data::program::MagicAtom;
use crate::data::relation::ColType;
use crate::data::symb::Symbol;
use crate::data::value::{DataValue, Num};
use crate::query::compile::IndexPositionUse;
use crate::runtime::relation::RelationHandle;

pub(crate) fn leading_len(positions: &[usize], key_len: usize) -> usize {
    (0..key_len).take_while(|i| positions.contains(i)).count()
}

/// The same classification drives eligibility, execution and explain output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StoredAccess {
    Point,
    Prefix,
    Materialized,
}

pub(crate) fn classify(positions: &[usize], key_len: usize, enabled: bool) -> StoredAccess {
    let mut sorted = positions.to_vec();
    sorted.sort_unstable();
    let contiguous = sorted.iter().copied().eq(0..sorted.len());
    if (enabled || contiguous) && leading_len(positions, key_len) == key_len {
        StoredAccess::Point
    } else if contiguous {
        StoredAccess::Prefix
    } else {
        StoredAccess::Materialized
    }
}

pub(crate) fn is_plain(store: &RelationHandle) -> bool {
    !store
        .metadata
        .keys
        .iter()
        .any(|c| matches!(c.typing.coltype, ColType::Validity | ColType::TxTime))
}

/// Vector/JSON equality is not guaranteed to agree with byte-key identity.
/// Any can contain those values. Keep existing seeks, but do not introduce
/// new seek-based equality for these types (including nested containers).
pub(crate) fn exact_key_type(ty: &ColType) -> bool {
    match ty {
        ColType::Bool
        | ColType::Int
        | ColType::Float
        | ColType::String
        | ColType::Bytes
        | ColType::Uuid => true,
        ColType::List { eltype, .. } => exact_key_type(&eltype.coltype),
        ColType::Tuple(types) => types.iter().all(|t| exact_key_type(&t.coltype)),
        _ => false,
    }
}

fn exact_key_value(value: &DataValue) -> bool {
    match value {
        DataValue::Null
        | DataValue::Bool(_)
        | DataValue::Num(_)
        | DataValue::Str(_)
        | DataValue::Bytes(_)
        | DataValue::Uuid(_) => true,
        DataValue::List(values) => values.iter().all(exact_key_value),
        _ => false,
    }
}

pub(crate) fn stored_access(
    store: &RelationHandle,
    positions: &[usize],
    filters: &[Expr],
    enabled: bool,
) -> StoredAccess {
    classify(
        positions,
        store.metadata.keys.len(),
        enabled
            && is_plain(store)
            && store
                .metadata
                .keys
                .iter()
                .all(|c| exact_key_type(&c.typing.coltype))
            && filters.iter().all(total_predicate),
    )
}

fn total_predicate(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Const {
            val: DataValue::Bool(_),
            ..
        }
    ) || matches!(expr, Expr::Apply { op, args, .. } if op.name == "OP_EQ" && args.iter().all(|a| matches!(a, Expr::Const { .. } | Expr::Binding { .. })))
}

fn safe_residual(atom: &MagicAtom) -> bool {
    match atom {
        MagicAtom::Predicate(expr) => total_predicate(expr),
        MagicAtom::Unification(u) => {
            if u.one_many_unif {
                if !deterministic(&u.expr) || !u.expr.bindings().is_ok_and(|b| b.is_empty()) {
                    return false;
                }
                let mut copy = u.expr.clone();
                return copy.partial_eval().is_ok()
                    && matches!(
                        copy,
                        Expr::Const {
                            val: DataValue::List(_),
                            ..
                        }
                    );
            }
            if matches!(&u.expr, Expr::Binding { .. } | Expr::Const { .. }) {
                return true;
            }
            if !deterministic(&u.expr) || !u.expr.bindings().is_ok_and(|b| b.is_empty()) {
                return false;
            }
            let mut copy = u.expr.clone();
            copy.partial_eval().is_ok() && matches!(copy, Expr::Const { .. })
        }
        _ => true,
    }
}

fn expression(atom: &MagicAtom) -> Option<&Expr> {
    match atom {
        MagicAtom::Predicate(expr) => Some(expr),
        MagicAtom::Unification(u) => Some(&u.expr),
        _ => None,
    }
}

struct Access {
    id: u64,
    prefix: usize,
    seek: bool,
    full_primary: bool,
    base_fetch: bool,
}

pub(crate) fn position_uses(args: &[Symbol], bound: &BTreeSet<Symbol>) -> Vec<IndexPositionUse> {
    let mut seen = bound.clone();
    args.iter()
        .map(|v| {
            if !seen.insert(v.clone()) {
                IndexPositionUse::Join
            } else if v.is_generated_ignored_symbol() {
                IndexPositionUse::Ignored
            } else {
                IndexPositionUse::BindForLater
            }
        })
        .collect()
}

fn access(store: &RelationHandle, args: &[Symbol], bound: &BTreeSet<Symbol>) -> Access {
    let uses = position_uses(args, bound);
    let (physical, mapper, base_fetch) = match store.choose_index(&uses, false) {
        Some((index, mapper, base_fetch)) => (index, mapper, base_fetch),
        None => (store.clone(), (0..args.len()).collect(), false),
    };
    let positions: Vec<_> = mapper
        .iter()
        .enumerate()
        .filter_map(|(i, &source)| (uses[source] == IndexPositionUse::Join).then_some(i))
        .collect();
    let key_len = physical.metadata.keys.len();
    let prefix = leading_len(&positions, key_len);
    Access {
        id: physical.id.0,
        prefix,
        seek: prefix > 0
            && stored_access(&physical, &positions, &[], true) != StoredAccess::Materialized,
        full_primary: physical.id == store.id && prefix == key_len,
        base_fetch,
    }
}

pub(crate) fn deterministic(expr: &Expr) -> bool {
    match expr {
        Expr::Const { .. } | Expr::Binding { .. } => true,
        Expr::Apply { op, args, .. } => op.deterministic && args.iter().all(deterministic),
        Expr::Cond { clauses, .. } => clauses
            .iter()
            .all(|(a, b)| deterministic(a) && deterministic(b)),
        _ => false,
    }
}

pub(crate) fn local_bindings(
    store: &RelationHandle,
    args: &[Symbol],
    body: &[MagicAtom],
    index: usize,
    bound: &BTreeSet<Symbol>,
) -> Vec<(Symbol, Expr)> {
    if !is_plain(store)
        || body
            .iter()
            .filter_map(expression)
            .any(|e| !deterministic(e))
    {
        return vec![];
    }
    let types: Vec<_> = store
        .metadata
        .keys
        .iter()
        .chain(&store.metadata.non_keys)
        .map(|c| &c.typing.coltype)
        .collect();
    let mut candidates = BTreeMap::new();
    let mut consider = |var: &Symbol, expr: &Expr| {
        if bound.contains(var) || candidates.contains_key(var) {
            return;
        }
        let Some(pos) = args.iter().position(|v| v == var) else {
            return;
        };
        if matches!(types[pos], ColType::Validity | ColType::TxTime) {
            return;
        }
        let producers = body
            .iter()
            .filter(|atom| {
                matches!(atom,
            MagicAtom::Relation(r) if r.args.contains(var))
            })
            .count();
        if producers != 1 {
            return;
        }
        let mut expr = expr.clone();
        if let Expr::Binding { var: source, .. } = &expr {
            if !bound.contains(source)
                || !exact_key_type(types[pos])
                || matches!(types[pos], ColType::Int | ColType::Float)
            {
                return;
            }
        } else {
            if !deterministic(&expr) || expr.bindings().map_or(true, |v| !v.is_empty()) {
                return;
            }
            if expr.partial_eval().is_err() {
                return;
            }
            let Expr::Const { val, .. } = &expr else {
                return;
            };
            if !exact_key_value(val) {
                return;
            }
            if let DataValue::Num(n) = val {
                if !matches!(
                    (n, types[pos]),
                    (Num::Int(_), ColType::Int) | (Num::Float(_), ColType::Float)
                ) {
                    return;
                }
                // Conservative numeric fence from the contract, even though
                // this stage no longer changes magic adornments.
                if body.iter().any(|atom| match atom {
                    MagicAtom::Rule(r) | MagicAtom::NegatedRule(r) => r.args.contains(var),
                    MagicAtom::HnswSearch(_)
                    | MagicAtom::FtsSearch(_)
                    | MagicAtom::LshSearch(_) => true,
                    _ => false,
                }) {
                    return;
                }
            }
        }
        candidates.insert(var.clone(), expr);
    };
    for atom in &body[index + 1..] {
        match atom {
            MagicAtom::Unification(u) if !u.one_many_unif => consider(&u.binding, &u.expr),
            MagicAtom::Predicate(Expr::Apply { op, args, .. })
                if op.name == "OP_EQ" && args.len() == 2 =>
            {
                if let Expr::Binding { var, .. } = &args[0] {
                    consider(var, &args[1]);
                }
                if let Expr::Binding { var, .. } = &args[1] {
                    consider(var, &args[0]);
                }
            }
            _ => {}
        }
    }
    if candidates.is_empty() {
        return vec![];
    }
    // Changing a scan into a seek can skip evaluations on nonmatching rows,
    // even for a residual that does not mention the newly bound key.
    if !body[index + 1..].iter().all(safe_residual) {
        return vec![];
    }
    // A noncandidate's folded RHS remains an expression in the baseline RA.
    // If it becomes a right-side filter it must satisfy the very same total
    // filter gate as runtime dispatch. Otherwise even an existing point lookup
    // could become a materialization after we add a nonadjacent join pair.
    if body[index + 1..].iter().any(|atom| {
        matches!(atom,
            MagicAtom::Unification(u) if !candidates.contains_key(&u.binding)
                && !matches!(&u.expr, Expr::Const { .. } | Expr::Binding { .. })
        )
    }) {
        return vec![];
    }
    let before = access(store, args, bound);
    let mut new_bound = bound.clone();
    new_bound.extend(candidates.keys().cloned());
    let after = access(store, args, &new_bound);
    if !after.seek
        || (before.seek
            && !after.full_primary
            && (before.id != after.id || after.prefix < before.prefix))
        || (before.seek && !before.base_fetch && after.base_fetch)
    {
        return vec![];
    }
    candidates.into_iter().collect()
}
