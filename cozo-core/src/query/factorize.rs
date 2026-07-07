/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Automatic factorized-count rewrite (mnestic fork, 0.10.5) — a normal-form
//! pre-pass that turns a single-clause `count()`-over-a-positive-join rule into
//! Yannakakis-style per-key counting sub-rules whose answer is **bit-identical**
//! to the naive enumeration, but which never enumerates the join's matches.
//!
//! Companion authoring spec: `docs/specs/cardinality-algebra.md`. Design memo:
//! `factorized-aggregation` (T3 v1). Two products live here:
//!
//! * **Item I — detector.** [`maybe_rewrite_and_advise`] always runs a pure
//!   analysis over the normalized program and, when the counted body factorizes
//!   over a separator, emits a `log::info!` advisory (and an `::explain` row).
//!   Never estimates cardinalities (the engine has no statistics).
//!
//! * **Item J — rewrite.** When the caller passes `enabled = true` (a Db-level
//!   kill switch, default OFF for this release) AND the body factorizes with a
//!   *provably exact* decomposition, the entry rule is replaced by synthesized
//!   sub-rules. When ANYTHING is unverifiable the pass declines and the program
//!   passes through untouched — biasing hard toward NOT firing, because a
//!   silently wrong count is the worst possible defect.
//!
//! # Why product-of-counts is exact here (the load-bearing proof)
//!
//! Two engine semantics carry the whole rewrite, and every firing condition
//! exists to keep one of them applicable (see `cardinality-algebra.md` §4):
//!
//! 1. **Rule stores and stored relations are SETS.** A single-clause conjunctive
//!    body over set atoms enumerates each full variable binding exactly once, so
//!    for a fixed valuation of a separator `S` the matches of the whole body are
//!    in bijection with the Cartesian product of the per-component match sets.
//! 2. **Aggregate input streams are BAGS.** The projection before an aggregate
//!    does not deduplicate, so `count` counts *body matches* and a per-key
//!    `count` sub-rule counts exactly the assignments to that component's private
//!    variables.
//!
//! **Decomposition theorem (applied recursively).** Pick a *central* atom `A`
//! whose variables `S = vars(A)` cover the required group keys. Split the other
//! atoms into connected components `C_1..C_m` by connectivity through variables
//! *not* in `S`. Then the components have pairwise-disjoint non-`S` variables, so
//! for every valuation `σ` of `S` that occurs in `A`,
//!
//! ```text
//!   #matches(σ) = Π_i  child_i(σ | (S ∩ vars(C_i)))
//! ```
//!
//! where `child_i` counts `C_i` keyed on the separator variables it touches.
//! Summing the product over the `A`-tuples, grouped by the required keys, is the
//! naive bag-count exactly. This holds for ANY hypergraph whenever the recursion
//! runs to completion — cyclic cores simply fail to find a covering central atom
//! for their interior keys and the pass declines. Integer exactness and overflow
//! safety are handled by the internal `int_sum_prod` aggregate (see `data/aggr.rs`).

use std::collections::{BTreeMap, BTreeSet};

use crate::data::aggr::{count_aggr, count_aggr_name, int_sum_prod_aggr};
use crate::data::expr::Expr;
use crate::data::functions::{OP_ADD, OP_LIST, OP_NEQ, OP_SUB};
use crate::data::program::{
    NormalFormAtom, NormalFormInlineRule, NormalFormProgram, NormalFormRelationApplyAtom,
    NormalFormRuleApplyAtom, NormalFormRulesOrFixed, Unification,
};
use crate::data::symb::{Symbol, PROG_ENTRY};
use crate::data::value::DataValue;
use crate::parse::SourceSpan;

/// Cap on synthesized-rule generation. The recursion depth is bounded by the
/// number of body atoms (each level removes the central atom), so this only ever
/// trips on a pathological program; it exists purely as a safety valve.
const SYNTH_BUDGET: usize = 100_000;

/// Cap on the number of `!=` inequalities handled by inclusion–exclusion. Two
/// inequalities already need four terms; beyond that the bookkeeping risk
/// outweighs the win (see `cardinality-algebra.md` §3.3).
const MAX_INEQUALITIES: usize = 2;

/// Entry point: run the detector (item I) and, when `enabled`, the rewrite
/// (item J). Returns the (possibly rewritten) program plus an optional advisory
/// message for `::explain`.
pub(crate) fn maybe_rewrite_and_advise(
    prog: NormalFormProgram,
    enabled: bool,
) -> (NormalFormProgram, Option<String>) {
    let analysis = match Analysis::extract(&prog) {
        Some(a) => a,
        None => return (prog, None),
    };

    // Item I — advisory. Independent of the kill switch: reporting that a count
    // factorizes over a separator is always safe (it changes nothing).
    let advisory = analysis.advisory();
    if let Some(msg) = &advisory {
        log::info!("query-factorization: {msg}");
    }

    // Item J — rewrite, only behind the kill switch and only when the full
    // recursive synthesis succeeds (i.e. is provably exact).
    if enabled {
        if let Some(new_prog) = analysis.synthesize(prog.disable_magic_rewrite) {
            return (new_prog, advisory);
        }
    }
    (prog, advisory)
}

/// The structural extraction of an eligible count rule. Holds the original head
/// layout (so the rewritten entry keeps the user's column order and arity), the
/// positive relation atoms, and the cross `!=` inequalities to be handled by
/// inclusion–exclusion.
struct Analysis {
    span: SourceSpan,
    /// Original head symbols, in the user's column order.
    orig_head: Vec<Symbol>,
    /// Index of the single `count()` position within the head.
    count_pos: usize,
    /// Group-by key symbols (deterministic, sorted order).
    group_keys: Vec<Symbol>,
    group_key_set: BTreeSet<Symbol>,
    /// The positive stored-relation atoms of the body.
    rel_atoms: Vec<NormalFormRelationApplyAtom>,
    /// The `!=` inequalities between two distinct variables (≤ [`MAX_INEQUALITIES`]).
    neqs: Vec<(Symbol, Symbol)>,
}

impl Analysis {
    /// Attempt to read a single non-recursive rule whose head is exactly one
    /// `count()` (optionally with group-by keys) over a positive stored-relation
    /// join. Returns `None` — decline — on anything outside that narrow shape.
    fn extract(prog: &NormalFormProgram) -> Option<Analysis> {
        // The whole program must be exactly one rule set, and it must be the
        // query entry `?`. (No rule-application atoms are permitted below, so
        // any other rule would be dead anyway; requiring a single rule keeps the
        // analysis self-contained.)
        if prog.prog.len() != 1 {
            return None;
        }
        let (name, rules_or_fixed) = prog.prog.iter().next()?;
        if !name.is_prog_entry() {
            return None;
        }
        let rules = rules_or_fixed.rules()?; // `None` for a fixed/algo rule.
        // Single clause only: a multi-clause aggregate rule streams every clause
        // into ONE accumulator, so a union-duplicated tuple is bag-counted twice
        // — a single-clause factorization cannot reproduce that.
        if rules.len() != 1 {
            return None;
        }
        let rule = &rules[0];

        // Head: exactly one `count` aggregate; every other position is a group
        // key. Reject `count_unique`, mixed/other aggregates, and a `count` with
        // extra parameters.
        let mut count_pos: Option<usize> = None;
        let mut group_key_set: BTreeSet<Symbol> = BTreeSet::new();
        for (i, a) in rule.aggr.iter().enumerate() {
            match a {
                None => {
                    group_key_set.insert(rule.head.get(i)?.clone());
                }
                Some((aggr, params)) => {
                    if count_pos.is_some() {
                        return None; // more than one aggregate
                    }
                    if aggr.name != count_aggr_name() {
                        return None; // count_unique / sum / ... — decline
                    }
                    if !params.is_empty() {
                        return None; // count(x, k) shape — decline
                    }
                    count_pos = Some(i);
                }
            }
        }
        let count_pos = count_pos?; // there must be exactly one aggregate

        // Body: positive relation atoms + `!=`(var, var) predicates, plus
        // droppable pure-output unifications (the `count(tup = [..])` shape).
        // Anything else declines.
        let mut rel_atoms: Vec<NormalFormRelationApplyAtom> = vec![];
        let mut neqs: Vec<(Symbol, Symbol)> = vec![];
        let mut unifs: Vec<&Unification> = vec![];
        for atom in &rule.body {
            match atom {
                NormalFormAtom::Relation(r) => {
                    // Bitemporal selectors resolve per-atom against the current
                    // transaction; keep the conservative v1 out of that
                    // interaction entirely.
                    if r.valid_at.is_some() || r.tx_valid_at.is_some() {
                        return None;
                    }
                    rel_atoms.push(r.clone());
                }
                NormalFormAtom::Predicate(expr) => match as_var_neq(expr) {
                    Some(pair) => neqs.push(pair),
                    None => return None, // any non-`!=` predicate — decline
                },
                NormalFormAtom::Unification(u) => unifs.push(u),
                // Negation, rule applications, and search atoms all break the
                // exactness conditions; decline.
                NormalFormAtom::NegatedRelation(_)
                | NormalFormAtom::NegatedRule(_)
                | NormalFormAtom::Rule(_)
                | NormalFormAtom::HnswSearch(_)
                | NormalFormAtom::FtsSearch(_)
                | NormalFormAtom::LshSearch(_) => return None,
            }
        }

        if rel_atoms.len() < 2 {
            return None; // nothing to factorize (a single scan is already optimal)
        }
        if neqs.len() > MAX_INEQUALITIES {
            return None;
        }

        // The set of variables the relation atoms bind (non-wildcard).
        let mut rel_vars: BTreeSet<Symbol> = BTreeSet::new();
        for a in &rel_atoms {
            rel_vars.extend(nonwild_vars(a));
        }

        // Every group key must be produced by a relation atom.
        if !group_key_set.is_subset(&rel_vars) {
            return None;
        }
        // Every inequality variable must be produced by a relation atom.
        for (u, v) in &neqs {
            if u == v || !rel_vars.contains(u) || !rel_vars.contains(v) {
                return None;
            }
        }

        // Validate the unifications are pure-output and droppable. A unification
        // is droppable iff it cannot change the answer or raise an error:
        //   * plain `=` (not `in` — a list unification generates rows),
        //   * its bound symbol is used nowhere that matters (not in a relation
        //     atom, not a group key, not in an inequality, not in another
        //     unification's binding or expression),
        //   * its expression is a non-erroring value packer (const / var /
        //     nested list of those) — never a computation that could fail.
        // This is exactly the `count(tup = [p1, p2, ...])` shape; anything else
        // (a real filter, an arithmetic bind, an `in`) declines.
        let other_unif_bindings: BTreeSet<Symbol> =
            unifs.iter().map(|u| u.binding.clone()).collect();
        for u in &unifs {
            if u.one_many_unif {
                return None;
            }
            let b = &u.binding;
            if rel_vars.contains(b) || group_key_set.contains(b) {
                return None;
            }
            if neqs.iter().any(|(x, y)| x == b || y == b) {
                return None;
            }
            if !is_nonerroring_value_expr(&u.expr) {
                return None;
            }
            // `b` must not feed any OTHER unification. (It feeds only the head
            // count position, whose value `count` ignores.)
            for other in &unifs {
                if std::ptr::eq(*other, *u) {
                    continue;
                }
                let refs = other.expr.bindings().ok()?;
                if refs.contains(b) {
                    return None;
                }
            }
            // And no unification's expr may reference another unification's
            // binding (drop them all cleanly, no chains).
            let refs = u.expr.bindings().ok()?;
            if refs.iter().any(|s| other_unif_bindings.contains(s)) {
                return None;
            }
        }

        // Inclusion–exclusion (the `!=` extension) is restricted to the KEYLESS
        // case in v1: keyed correction terms need explicit missing-group defaults
        // (`cardinality-algebra.md` §4.5), which is deferred. Pure factorization
        // (no `!=`) supports group-by keys.
        if !neqs.is_empty() && !group_key_set.is_empty() {
            return None;
        }

        let group_keys: Vec<Symbol> = group_key_set.iter().cloned().collect();
        Some(Analysis {
            span: rule.head.first().map(|s| s.span).unwrap_or(SourceSpan(0, 0)),
            orig_head: rule.head.clone(),
            count_pos,
            group_keys,
            group_key_set,
            rel_atoms,
            neqs,
        })
    }

    /// Item I: a `log::info!` / `::explain` advisory when the counted body
    /// factorizes over a separator (a covering central atom whose removal splits
    /// the rest into ≥ 2 independent components). Reports the separator only —
    /// no cardinality estimate.
    fn advisory(&self) -> Option<String> {
        let central_idx = choose_central(&self.rel_atoms, &self.group_key_set)?;
        let sep = nonwild_vars(&self.rel_atoms[central_idx]);
        let comps = components_excluding(&self.rel_atoms, central_idx, &sep);
        if comps.len() < 2 {
            return None; // no genuine multi-way split
        }
        let sep_list = sep
            .iter()
            .map(|s| s.name.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let ie = if self.neqs.is_empty() {
            String::new()
        } else {
            format!(
                " (with {} inclusion–exclusion inequality term(s))",
                self.neqs.len()
            )
        };
        Some(format!(
            "rule `?`: count over a materialized join; factorizes over separator \
             {{{sep_list}}} into {} components{ie} — see docs/specs/cardinality-algebra.md",
            comps.len()
        ))
    }

    /// Item J: synthesize the full rewritten program, or `None` if any part of
    /// the decomposition cannot be guaranteed exact.
    fn synthesize(&self, disable_magic_rewrite: bool) -> Option<NormalFormProgram> {
        let mut gen = NameGen::new(SYNTH_BUDGET);
        let mut prog: BTreeMap<Symbol, NormalFormRulesOrFixed> = BTreeMap::new();

        let (entry_head, entry_aggr, entry_body) = if self.neqs.is_empty() {
            // --- pure factorization over the group keys --------------------
            let (top_name, rules) =
                factorize_count(&self.rel_atoms, &self.group_key_set, self.span, &mut gen)?;
            insert_rules(&mut prog, rules);

            let r_var = gen.fresh_var(self.span);
            let mut args: Vec<Symbol> = self.group_keys.clone(); // sorted key order
            args.push(r_var.clone());
            let body = vec![NormalFormAtom::Rule(NormalFormRuleApplyAtom {
                name: top_name,
                args,
                span: self.span,
            })];
            let (head, aggr) = self.entry_head_layout(&r_var);
            (head, aggr, body)
        } else {
            // --- keyless `!=` inclusion–exclusion --------------------------
            //   count(∧ u_i ≠ v_i) = Σ_{T ⊆ ineqs} (−1)^|T| count(body with the
            //   pairs in T identified). Each term is factorized from scratch.
            let k = self.neqs.len();
            let mut body: Vec<NormalFormAtom> = vec![];
            let mut signed_terms: Vec<(bool, Symbol)> = vec![]; // (is_positive, count var)
            for mask in 0u32..(1u32 << k) {
                let mut identify: Vec<(Symbol, Symbol)> = vec![];
                for (i, pair) in self.neqs.iter().enumerate() {
                    if mask & (1 << i) != 0 {
                        identify.push(pair.clone());
                    }
                }
                let subst = substitute_atoms(&self.rel_atoms, &identify);
                let (term_name, rules) =
                    factorize_count(&subst, &BTreeSet::new(), self.span, &mut gen)?;
                insert_rules(&mut prog, rules);

                let c_var = gen.fresh_var(self.span);
                body.push(NormalFormAtom::Rule(NormalFormRuleApplyAtom {
                    name: term_name,
                    args: vec![c_var.clone()],
                    span: self.span,
                }));
                let is_positive = identify.len() % 2 == 0;
                signed_terms.push((is_positive, c_var));
            }
            let r_var = gen.fresh_var(self.span);
            body.push(NormalFormAtom::Unification(Unification {
                binding: r_var.clone(),
                expr: build_signed_sum(&signed_terms, self.span),
                one_many_unif: false,
                span: self.span,
            }));
            let (head, aggr) = self.entry_head_layout(&r_var);
            (head, aggr, body)
        };

        prog.insert(
            Symbol::new(PROG_ENTRY, self.span),
            NormalFormRulesOrFixed::Rules {
                rules: vec![NormalFormInlineRule {
                    head: entry_head,
                    aggr: entry_aggr,
                    body: entry_body,
                }],
            },
        );

        Some(NormalFormProgram {
            prog,
            disable_magic_rewrite,
        })
    }

    /// Build the entry rule's head in the ORIGINAL column layout: every group
    /// key stays where the user wrote it, and the single count position carries
    /// the result variable. The entry is a plain (non-aggregate) rule — all its
    /// head positions are `None`.
    fn entry_head_layout(
        &self,
        r_var: &Symbol,
    ) -> (
        Vec<Symbol>,
        Vec<Option<(crate::data::aggr::Aggregation, Vec<DataValue>)>>,
    ) {
        let mut head = Vec::with_capacity(self.orig_head.len());
        for (i, sym) in self.orig_head.iter().enumerate() {
            if i == self.count_pos {
                head.push(r_var.clone());
            } else {
                head.push(sym.clone());
            }
        }
        let aggr = vec![None; self.orig_head.len()];
        (head, aggr)
    }
}

/// A synthesized rule paired with its (fresh, unique) name.
struct NamedRule {
    name: Symbol,
    rule: NormalFormInlineRule,
}

fn insert_rules(prog: &mut BTreeMap<Symbol, NormalFormRulesOrFixed>, rules: Vec<NamedRule>) {
    for nr in rules {
        prog.insert(nr.name, NormalFormRulesOrFixed::Rules { rules: vec![nr.rule] });
    }
}

/// The recursive Yannakakis-style counting synthesis. Given a set of positive
/// relation atoms and the keys that must be preserved as group columns, returns
/// `(rule_name, all_rules)` where `rule_name` computes
/// `[sorted(required_keys)..., count]` — or `None` when the shape cannot be
/// factorized with a provably exact decomposition (e.g. a cyclic core, or a
/// required key that no single atom covers).
fn factorize_count(
    atoms: &[NormalFormRelationApplyAtom],
    required_keys: &BTreeSet<Symbol>,
    span: SourceSpan,
    gen: &mut NameGen,
) -> Option<(Symbol, Vec<NamedRule>)> {
    gen.tick()?;
    if atoms.is_empty() {
        return None;
    }
    // A relation atom that binds the same non-wildcard variable to two columns
    // (`*rel[x, x]`) is an intra-atom self-equality. Normal-form conversion never
    // ran on these synthesized bodies, and the compiler's duplicate-binding
    // handling breaks when such an atom is then joined to a synthesized rule
    // (the join key resolves against a renamed column). It can also arise from
    // an inclusion–exclusion substitution that identifies two variables which
    // co-occur in one atom. Both are rare and both decline safely — the diagonal
    // of every measured `!=` case (Q5/Q6) keeps the two inequality variables in
    // different atoms, so this never blocks a target.
    if atoms.iter().any(has_repeated_var) {
        return None;
    }

    // --- base case: a single atom -----------------------------------------
    // `cnt[keys..., count(one)] := *rel[..], one = 1`. Counting `one = 1`
    // (rather than a body variable) sidesteps every edge case: a head symbol
    // that coincides with a key, an all-wildcard atom, etc. `count` ignores the
    // value; the result is the number of atom tuples per key group, which — the
    // atom being a set — is the number of assignments to its non-key variables.
    if atoms.len() == 1 {
        let a = &atoms[0];
        let avars = nonwild_vars(a);
        if !required_keys.is_subset(&avars) {
            return None; // invariant violated: cannot key this component
        }
        let name = gen.next_rule(span);
        let one = gen.fresh_var(span);
        let keys = sorted(required_keys);
        let mut head = keys.clone();
        head.push(one.clone());
        let mut aggr = vec![None; keys.len()];
        aggr.push(Some((count_aggr(), vec![])));
        let body = vec![
            NormalFormAtom::Relation(a.clone()),
            NormalFormAtom::Unification(Unification {
                binding: one,
                expr: Expr::Const {
                    val: DataValue::from(1i64),
                    span,
                },
                one_many_unif: false,
                span,
            }),
        ];
        return Some((
            name.clone(),
            vec![NamedRule {
                name,
                rule: NormalFormInlineRule { head, aggr, body },
            }],
        ));
    }

    // --- recursive case: pick a central atom covering the required keys ----
    let central_idx = choose_central(atoms, required_keys)?;
    let central = atoms[central_idx].clone();
    let sep = nonwild_vars(&central);
    let comps = components_excluding(atoms, central_idx, &sep);

    let mut all_rules: Vec<NamedRule> = vec![];
    let mut children: Vec<(Symbol, Vec<Symbol>)> = vec![]; // (name, sorted child keys)
    for comp in &comps {
        // The keys a component must preserve are exactly the separator variables
        // it touches (all of which the central atom binds, so the combine join
        // is a functional lookup).
        let comp_vars: BTreeSet<Symbol> = comp.iter().flat_map(nonwild_vars).collect();
        let child_keys: BTreeSet<Symbol> = sep.intersection(&comp_vars).cloned().collect();
        let (child_name, child_rules) = factorize_count(comp, &child_keys, span, gen)?;
        all_rules.extend(child_rules);
        children.push((child_name, sorted(&child_keys)));
    }

    // combine rule:
    //   name[required_keys..., int_sum_prod(lst)] :=
    //       *central[sep..], child_i[child_keys_i.., c_i], ..., lst = [c_1, ..].
    let name = gen.next_rule(span);
    let keys = sorted(required_keys);
    let mut body: Vec<NormalFormAtom> = vec![NormalFormAtom::Relation(central)];
    let mut factor_exprs: Vec<Expr> = vec![];
    for (child_name, child_keys) in &children {
        let c_var = gen.fresh_var(span);
        let mut args = child_keys.clone();
        args.push(c_var.clone());
        body.push(NormalFormAtom::Rule(NormalFormRuleApplyAtom {
            name: child_name.clone(),
            args,
            span,
        }));
        factor_exprs.push(Expr::Binding {
            var: c_var,
            tuple_pos: None,
        });
    }
    let lst = gen.fresh_var(span);
    body.push(NormalFormAtom::Unification(Unification {
        binding: lst.clone(),
        expr: Expr::Apply {
            op: &OP_LIST,
            args: factor_exprs.into_boxed_slice(),
            span,
        },
        one_many_unif: false,
        span,
    }));
    let mut head = keys.clone();
    head.push(lst);
    let mut aggr = vec![None; keys.len()];
    aggr.push(Some((int_sum_prod_aggr(), vec![])));
    all_rules.push(NamedRule {
        name: name.clone(),
        rule: NormalFormInlineRule { head, aggr, body },
    });
    Some((name, all_rules))
}

/// Choose the central atom: among atoms whose variables cover `required_keys`,
/// the one whose removal yields the most components (best factorization),
/// tie-broken by written order for determinism. `None` if no atom covers the
/// required keys (a non-free-connex or cyclic shape — decline).
fn choose_central(
    atoms: &[NormalFormRelationApplyAtom],
    required_keys: &BTreeSet<Symbol>,
) -> Option<usize> {
    let mut best: Option<(usize, usize)> = None; // (component count, index)
    for (i, a) in atoms.iter().enumerate() {
        let avars = nonwild_vars(a);
        if !required_keys.is_subset(&avars) {
            continue;
        }
        let n = components_excluding(atoms, i, &avars).len();
        match best {
            None => best = Some((n, i)),
            Some((bn, _)) if n > bn => best = Some((n, i)),
            _ => {}
        }
    }
    best.map(|(_, i)| i)
}

/// The connected components of `atoms \ {central_idx}` where two atoms are
/// connected iff they share a non-wildcard variable that is NOT in `sep`.
fn components_excluding(
    atoms: &[NormalFormRelationApplyAtom],
    central_idx: usize,
    sep: &BTreeSet<Symbol>,
) -> Vec<Vec<NormalFormRelationApplyAtom>> {
    let other_indices: Vec<usize> = (0..atoms.len()).filter(|i| *i != central_idx).collect();
    let n = other_indices.len();

    // Per-other-atom private (non-separator) variable sets.
    let priv_vars: Vec<BTreeSet<Symbol>> = other_indices
        .iter()
        .map(|&idx| {
            nonwild_vars(&atoms[idx])
                .into_iter()
                .filter(|v| !sep.contains(v))
                .collect()
        })
        .collect();

    // Union–find over the `other_indices` positions.
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut [usize], x: usize) -> usize {
        let mut r = x;
        while parent[r] != r {
            r = parent[r];
        }
        let mut c = x;
        while parent[c] != r {
            let nxt = parent[c];
            parent[c] = r;
            c = nxt;
        }
        r
    }
    for i in 0..n {
        for j in (i + 1)..n {
            if priv_vars[i].intersection(&priv_vars[j]).next().is_some() {
                let ri = find(&mut parent, i);
                let rj = find(&mut parent, j);
                if ri != rj {
                    parent[ri] = rj;
                }
            }
        }
    }

    // Group by root, preserving deterministic (min-index-first) order.
    let mut groups: BTreeMap<usize, Vec<NormalFormRelationApplyAtom>> = BTreeMap::new();
    for (k, &idx) in other_indices.iter().enumerate() {
        let root = find(&mut parent, k);
        groups.entry(root).or_default().push(atoms[idx].clone());
    }
    groups.into_values().collect()
}

/// Apply variable identifications (union–find) to a set of atoms: every atom
/// argument is rewritten to the lexicographically-smallest name in its class.
/// Used to build the inclusion–exclusion correction terms — identifying `u` with
/// `v` produces the body of `count(body with u = v)`.
fn substitute_atoms(
    atoms: &[NormalFormRelationApplyAtom],
    identify: &[(Symbol, Symbol)],
) -> Vec<NormalFormRelationApplyAtom> {
    if identify.is_empty() {
        return atoms.to_vec();
    }
    // Collect the variables involved and union them.
    let mut parent: BTreeMap<Symbol, Symbol> = BTreeMap::new();
    fn find(parent: &mut BTreeMap<Symbol, Symbol>, x: &Symbol) -> Symbol {
        let mut r = x.clone();
        loop {
            let p = parent.get(&r).cloned().unwrap_or_else(|| r.clone());
            if p == r {
                return r;
            }
            r = p;
        }
    }
    for (u, v) in identify {
        parent.entry(u.clone()).or_insert_with(|| u.clone());
        parent.entry(v.clone()).or_insert_with(|| v.clone());
        let ru = find(&mut parent, u);
        let rv = find(&mut parent, v);
        if ru != rv {
            // Point the larger name at the smaller so `find` yields the min.
            let (lo, hi) = if ru < rv { (ru, rv) } else { (rv, ru) };
            parent.insert(hi, lo);
        }
    }
    // Resolve every seen variable to its class minimum.
    let seen: Vec<Symbol> = parent.keys().cloned().collect();
    let mut rep: BTreeMap<Symbol, Symbol> = BTreeMap::new();
    for s in seen {
        let r = find(&mut parent, &s);
        rep.insert(s, r);
    }
    atoms
        .iter()
        .map(|a| {
            let mut a = a.clone();
            for arg in a.args.iter_mut() {
                if let Some(r) = rep.get(arg) {
                    *arg = r.clone();
                }
            }
            a
        })
        .collect()
}

/// Build `Σ (+terms) − Σ (−terms)` as an `Expr` over the per-term count
/// variables. All terms are exact `i64` counts and the result is a non-negative
/// count bounded by the largest term, so `OP_ADD`/`OP_SUB` (which keep `Int` for
/// `Int` inputs) stay within range whenever the terms themselves fit `i64`.
fn build_signed_sum(signed_terms: &[(bool, Symbol)], span: SourceSpan) -> Expr {
    let binding = |s: &Symbol| Expr::Binding {
        var: s.clone(),
        tuple_pos: None,
    };
    let pos: Vec<Expr> = signed_terms
        .iter()
        .filter(|(p, _)| *p)
        .map(|(_, s)| binding(s))
        .collect();
    let neg: Vec<Expr> = signed_terms
        .iter()
        .filter(|(p, _)| !*p)
        .map(|(_, s)| binding(s))
        .collect();
    let pos_sum = Expr::Apply {
        op: &OP_ADD,
        args: pos.into_boxed_slice(),
        span,
    };
    if neg.is_empty() {
        return pos_sum;
    }
    let neg_sum = Expr::Apply {
        op: &OP_ADD,
        args: neg.into_boxed_slice(),
        span,
    };
    Expr::Apply {
        op: &OP_SUB,
        args: Box::new([pos_sum, neg_sum]),
        span,
    }
}

/// If `expr` is `neq(u, v)` for two distinct bare variables, return `(u, v)`.
fn as_var_neq(expr: &Expr) -> Option<(Symbol, Symbol)> {
    if let Expr::Apply { op, args, .. } = expr {
        if op.name == OP_NEQ.name && args.len() == 2 {
            if let (Expr::Binding { var: u, .. }, Expr::Binding { var: v, .. }) =
                (&args[0], &args[1])
            {
                if u != v {
                    return Some((u.clone(), v.clone()));
                }
            }
        }
    }
    None
}

/// A value expression that can never raise an error and only packs values:
/// a constant, a variable, or a (possibly nested) list of those. Only such
/// expressions may back a droppable pure-output unification.
fn is_nonerroring_value_expr(expr: &Expr) -> bool {
    match expr {
        Expr::Const { .. } | Expr::Binding { .. } => true,
        Expr::Apply { op, args, .. } => {
            op.name == OP_LIST.name && args.iter().all(is_nonerroring_value_expr)
        }
        _ => false,
    }
}

/// Non-wildcard variables of a relation atom.
fn nonwild_vars(a: &NormalFormRelationApplyAtom) -> BTreeSet<Symbol> {
    a.args
        .iter()
        .filter(|s| !s.is_generated_ignored_symbol())
        .cloned()
        .collect()
}

/// Whether a relation atom binds the same non-wildcard variable to two columns.
fn has_repeated_var(a: &NormalFormRelationApplyAtom) -> bool {
    let mut seen = BTreeSet::new();
    for s in &a.args {
        if s.is_generated_ignored_symbol() {
            continue;
        }
        if !seen.insert(s.clone()) {
            return true;
        }
    }
    false
}

fn sorted(set: &BTreeSet<Symbol>) -> Vec<Symbol> {
    set.iter().cloned().collect() // BTreeSet already yields sorted, deterministic order
}

/// Deterministic fresh-name generator for synthesized rules and variables. Both
/// use a `*`-prefix, which no user identifier can contain (`*` is the
/// stored-relation sigil), and which is neither a temp-store (`_`) nor a
/// generated-ignored (`~`) marker — so the names never collide with anything the
/// user wrote and are never mistaken for wildcards.
struct NameGen {
    rule_ctr: usize,
    var_ctr: usize,
    budget: usize,
}

impl NameGen {
    fn new(budget: usize) -> Self {
        Self {
            rule_ctr: 0,
            var_ctr: 0,
            budget,
        }
    }
    fn tick(&mut self) -> Option<()> {
        if self.budget == 0 {
            return None;
        }
        self.budget -= 1;
        Some(())
    }
    fn next_rule(&mut self, span: SourceSpan) -> Symbol {
        let s = Symbol::new(format!("*fac{}", self.rule_ctr), span);
        self.rule_ctr += 1;
        s
    }
    fn fresh_var(&mut self, span: SourceSpan) -> Symbol {
        let s = Symbol::new(format!("*fv{}", self.var_ctr), span);
        self.var_ctr += 1;
        s
    }
}
