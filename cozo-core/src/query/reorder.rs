/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::collections::BTreeSet;
use std::mem;

use miette::{bail, Diagnostic, Result};
use thiserror::Error;

use crate::data::expr::Expr;
use crate::data::program::{NormalFormAtom, NormalFormInlineRule, Unification};
use crate::data::symb::Symbol;
use crate::data::value::DataValue;
use crate::parse::SourceSpan;

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
            NormalFormAtom::Predicate(expr) => match eq_predicate_as_unification(&expr, &generated) {
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
            let numeric_const =
                matches!(maybe_ground, Expr::Const { val: DataValue::Num(_), .. });
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
    pub(crate) fn convert_to_well_ordered_rule(self) -> Result<Self> {
        let mut seen_variables = BTreeSet::default();
        let mut round_1_collected = vec![];
        let mut pending = vec![];

        // mnestic #1: rewrite equality post-filters on stored relations into
        // hoisted bindings so keyed lookups compile to prefix joins.
        let body = push_equality_filters_to_bindings(self.body);

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

        Ok(NormalFormInlineRule {
            head: self.head,
            aggr: self.aggr,
            body: collected,
        })
    }
}
