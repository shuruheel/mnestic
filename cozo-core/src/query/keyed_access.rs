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
        seek: prefix > 0 && classify(&positions, key_len, true) != StoredAccess::Materialized,
        full_primary: physical.id == store.id && prefix == key_len,
        base_fetch,
    }
}

fn deterministic(expr: &Expr) -> bool {
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
    if !is_plain(store) {
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
                || matches!(types[pos], ColType::Any | ColType::Int | ColType::Float)
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
