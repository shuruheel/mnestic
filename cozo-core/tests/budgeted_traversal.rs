/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `BudgetedTraversal` — the spec §11 edge-case matrix
//! (`docs/specs/budgeted-traversal.md`), rows (a)–(o) plus the witness row
//! (w1)–(w3) added by amendment A2, plus the out-of-tree
//! `supports_projection` registration probe adopted from amendment A5.
//!
//! Sqlite backend per the repo test-backend rule. Every fixture weight is
//! f32-exact (the CSR stores weights as f32), so every expected cost is an
//! exact f64 and raw-row `assert_eq!` is sound. Expected vectors are written
//! in the store's canonical order (the output temp store is a `BTreeMap` over
//! whole tuples — canonical order IS part of the contract under test).

#![cfg(feature = "graph-algo")]

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use cozo::{
    DataValue, DbInstance, Expr, FixedRule, FixedRulePayload, NamedRows, Poison, RegularTempStore,
    ScriptMutability, SourceSpan, Symbol, VariantSpec,
};

fn open_db() -> (tempfile::TempDir, DbInstance) {
    let dir = tempfile::tempdir().unwrap();
    let db = DbInstance::new("sqlite", dir.path().join("bt.db").to_str().unwrap(), "").unwrap();
    (dir, db)
}

fn run(db: &DbInstance, s: &str) -> NamedRows {
    db.run_script(s, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("script failed: {s}\n{e:?}"))
}

/// miette's fancy Debug rendering line-wraps messages with box-drawing
/// decorations; collapse to one plain line so `contains` checks are robust.
fn errstr(e: &impl std::fmt::Debug) -> String {
    format!("{e:?}")
        .replace(['\u{2502}', '\u{d7}'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn err(db: &DbInstance, s: &str) -> String {
    let e = db
        .run_script(s, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_err();
    errstr(&e)
}

fn err_with(db: &DbInstance, s: &str, params: BTreeMap<String, DataValue>) -> String {
    let e = db
        .run_script(s, params, ScriptMutability::Mutable)
        .unwrap_err();
    errstr(&e)
}

/// `(node, cost, parent, depth)` — typed row for hand-written expectations.
fn row(n: &str, c: f64, p: Option<&str>, d: i64) -> Vec<DataValue> {
    vec![
        DataValue::from(n),
        DataValue::from(c),
        match p {
            Some(p) => DataValue::from(p),
            None => DataValue::Null,
        },
        DataValue::from(d),
    ]
}

fn node_set(res: &NamedRows) -> BTreeSet<String> {
    res.rows
        .iter()
        .map(|r| match &r[0] {
            DataValue::Str(s) => s.to_string(),
            other => panic!("non-string node {other:?}"),
        })
        .collect()
}

/// Fixture (a): diamond with a tail. Settle order s-free hand check:
/// a(0) → b(1) → c(2 via b, beats the 4.0 direct edge) → d(3 via c, beats 6 via b).
const DIAMOND_TAIL: &str = "e[f, t, w] <- [['a','b',1.0],['a','c',4.0],['b','c',1.0],\
                            ['b','d',5.0],['c','d',1.0]]\ns[n] <- [['a']]\n";

fn diamond_tail_expected() -> Vec<Vec<DataValue>> {
    vec![
        row("a", 0.0, None, 0),
        row("b", 1.0, Some("a"), 1),
        row("c", 2.0, Some("b"), 2),
        row("d", 3.0, Some("c"), 3),
    ]
}

// ---------------------------------------------------------------------------
// (a) known answer
// ---------------------------------------------------------------------------

#[test]
fn a_known_answer_diamond_with_tail() {
    let (_dir, db) = open_db();
    let res = run(
        &db,
        &format!(
            "{DIAMOND_TAIL}?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], max_nodes: 10)"
        ),
    );
    assert_eq!(res.rows, diamond_tail_expected());
    // One row per node (spec §5 invariant).
    assert_eq!(node_set(&res).len(), res.rows.len());
}

// ---------------------------------------------------------------------------
// (b) budget cuts
// ---------------------------------------------------------------------------

#[test]
fn b_budget_cut_exact_prefix() {
    let (_dir, db) = open_db();
    let res = run(
        &db,
        &format!(
            "{DIAMOND_TAIL}?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], max_nodes: 3)"
        ),
    );
    assert_eq!(res.rows, diamond_tail_expected()[..3].to_vec());
    assert!(!node_set(&res).contains("d"));

    // Seeds count toward the budget: max_nodes 1 admits only the seed.
    let res = run(
        &db,
        &format!(
            "{DIAMOND_TAIL}?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], max_nodes: 1)"
        ),
    );
    assert_eq!(res.rows, vec![row("a", 0.0, None, 0)]);
}

#[test]
fn b_budget_smaller_than_seed_set() {
    let (_dir, db) = open_db();
    // No edges at all: both seeds are loose (CSR-absent) roots; the budget
    // cut between the equal-cost seeds falls to DataValue order.
    run(&db, ":create ee {f: String, t: String => w: Float}");
    let q = |mn: usize| {
        run(
            &db,
            &format!(
                "s[n] <- [['s1'],['s2']]\n?[n, c, p, d] <~ BudgetedTraversal(*ee[f, t, w], s[n], max_nodes: {mn})"
            ),
        )
    };
    assert_eq!(q(1).rows, vec![row("s1", 0.0, None, 0)]);
    assert_eq!(
        q(2).rows,
        vec![row("s1", 0.0, None, 0), row("s2", 0.0, None, 0)]
    );
}

// ---------------------------------------------------------------------------
// (c) boundary ties break on the node VALUE, never a CSR id
// ---------------------------------------------------------------------------

/// The dummy edge `a→n3 9.0` is load-bearing for the §11 discrimination run:
/// inputs scan in BTree order, so sorted-scan interning gives `n3` a low u32
/// id (a=0, n3=1, s=2, n1=3, …) and a u32 tie-break would admit {n3, n1}
/// instead of {n1, n2}. The weight 9.0 never wins, and `a` is unreachable, so
/// the canonical answer is unchanged.
const TIE_STAR: &str = "e[f, t, w] <- [['s','n1',1.0],['s','n2',1.0],['s','n3',1.0],\
                        ['s','n4',1.0],['a','n3',9.0]]\ns[n] <- [['s']]\n";

#[test]
fn c_boundary_tie_admission_by_datavalue_order() {
    let (_dir, db) = open_db();
    let res = run(
        &db,
        &format!("{TIE_STAR}?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], max_nodes: 3)"),
    );
    assert_eq!(
        res.rows,
        vec![
            row("n1", 1.0, Some("s"), 1),
            row("n2", 1.0, Some("s"), 1),
            row("s", 0.0, None, 0),
        ]
    );
}

// ---------------------------------------------------------------------------
// (d) confluence under permuted input literals (contract pin — inline const
// rules are set-semantic, so this is not the interning discriminator; (e) is)
// ---------------------------------------------------------------------------

#[test]
fn d_confluence_under_permuted_inputs() {
    let (_dir, db) = open_db();

    // (d1) the tie star, three literal orders.
    let star_perms = [
        "[['s','n1',1.0],['s','n2',1.0],['s','n3',1.0],['s','n4',1.0],['a','n3',9.0]]",
        "[['a','n3',9.0],['s','n4',1.0],['s','n3',1.0],['s','n2',1.0],['s','n1',1.0]]",
        "[['s','n3',1.0],['a','n3',9.0],['s','n1',1.0],['s','n4',1.0],['s','n2',1.0]]",
    ];
    let star_expected = vec![
        row("n1", 1.0, Some("s"), 1),
        row("n2", 1.0, Some("s"), 1),
        row("s", 0.0, None, 0),
    ];
    for p in star_perms {
        let res = run(
            &db,
            &format!(
                "e[f, t, w] <- {p}\ns[n] <- [['s']]\n?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], max_nodes: 3)"
            ),
        );
        assert_eq!(res.rows, star_expected, "star permutation {p}");
    }

    // (d2) equal-cost two-parent diamond: t's witness must be (3.0, "a", 2) —
    // the (3.0, 2, "b") label does not improve (3.0, 2, "a") under strict
    // lexicographic relaxation.
    let diamond_perms = [
        "[['s','a',2.0],['s','b',2.0],['a','t',1.0],['b','t',1.0]]",
        "[['b','t',1.0],['a','t',1.0],['s','b',2.0],['s','a',2.0]]",
        "[['s','b',2.0],['a','t',1.0],['s','a',2.0],['b','t',1.0]]",
    ];
    let diamond_expected = vec![
        row("a", 2.0, Some("s"), 1),
        row("b", 2.0, Some("s"), 1),
        row("s", 0.0, None, 0),
        row("t", 3.0, Some("a"), 2),
    ];
    for p in diamond_perms {
        let res = run(
            &db,
            &format!(
                "e[f, t, w] <- {p}\ns[n] <- [['s']]\n?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], max_nodes: 10)"
            ),
        );
        assert_eq!(res.rows, diamond_expected, "diamond permutation {p}");
    }
}

// ---------------------------------------------------------------------------
// (e) form equivalence: positional ≡ graph: ≡ graph:+nodes:, ties included
// ---------------------------------------------------------------------------

/// Union of the tie star and the two-parent diamond, budget 3 — the budget
/// cuts INSIDE the equal-cost group {n1..n4}, which is what makes an
/// interning-dependent tie-break observable: the positional form interns in
/// sorted edge-scan order (a=0, n3=1, t=2, b=3, s=4, n1=5, …) while the
/// `nodes:` form interns the node list first (a=0, b=1, n1=2, …) — a u32
/// tie-break admits {n3, n1} on one form and {n1, n2} on the other.
#[test]
fn e_form_equivalence_positional_graph_and_nodes_slot() {
    let (_dir, db) = open_db();
    run(&db, ":create e {f: String, t: String => w: Float}");
    run(
        &db,
        "?[f, t, w] <- [['s','n1',1.0],['s','n2',1.0],['s','n3',1.0],['s','n4',1.0],\
         ['s','a',2.0],['s','b',2.0],['a','t',1.0],['b','t',1.0],['a','n3',9.0]] \
         :put e {f, t => w}",
    );
    run(&db, ":create nl {x: String}");
    run(
        &db,
        "?[x] <- [['a'],['b'],['n1'],['n2'],['n3'],['n4'],['s'],['t']] :put nl {x}",
    );
    run(&db, "::graph create g1 {edges: e}");
    run(&db, "::graph create g2 {edges: e, nodes: nl}");

    let expected = vec![
        row("n1", 1.0, Some("s"), 1),
        row("n2", 1.0, Some("s"), 1),
        row("s", 0.0, None, 0),
    ];
    let queries = [
        "s[n] <- [['s']]\n?[n, c, p, d] <~ BudgetedTraversal(*e[f, t, w], s[n], max_nodes: 3)",
        "s[n] <- [['s']]\n?[n, c, p, d] <~ BudgetedTraversal(s[n], graph: 'g1', max_nodes: 3)",
        "s[n] <- [['s']]\n?[n, c, p, d] <~ BudgetedTraversal(s[n], graph: 'g2', max_nodes: 3)",
        // Non-binding max_depth extends form equivalence over the layered loop
        // (strictly positive weights, so A1's byte-equality claim applies).
        "s[n] <- [['s']]\n?[n, c, p, d] <~ BudgetedTraversal(s[n], graph: 'g1', max_nodes: 3, max_depth: 3)",
    ];
    for q in queries {
        assert_eq!(run(&db, q).rows, expected, "form: {q}");
    }
}

// ---------------------------------------------------------------------------
// (f) cost ceiling prunes paths, not nodes; over-ceiling seeds spend nothing
// ---------------------------------------------------------------------------

#[test]
fn f_cost_ceiling_prunes_paths_not_nodes() {
    let (_dir, db) = open_db();
    // x's direct edge (5.0) is over the ceiling but the two-hop path (3.0) is
    // not: x must be admitted at 3.0. An implementation that discards the
    // NODE on seeing one over-ceiling path goes red.
    let res = run(
        &db,
        "e[f, t, w] <- [['s','x',5.0],['s','y',1.0],['y','x',2.0]]\ns[n] <- [['s']]\n\
         ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], max_nodes: 10, max_cost: 4.0)",
    );
    assert_eq!(
        res.rows,
        vec![
            row("s", 0.0, None, 0),
            row("x", 3.0, Some("y"), 2),
            row("y", 1.0, Some("s"), 1),
        ]
    );
}

#[test]
fn f_seed_over_ceiling_inadmissible() {
    let (_dir, db) = open_db();
    // s2 (initial_cost 9.0 > max_cost 4.0) is skipped entirely: no row, no
    // budget slot, and z — reachable only through s2 — must be absent, which
    // proves the seed expanded nothing.
    let res = run(
        &db,
        "e[f, t, w] <- [['s2','z',0.5]]\ns[n, c] <- [['s1', 0.0], ['s2', 9.0]]\n\
         ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n, c], max_nodes: 2, max_cost: 4.0)",
    );
    assert_eq!(res.rows, vec![row("s1", 0.0, None, 0)]);
}

// ---------------------------------------------------------------------------
// (g) depth exactness — layered labels, never depth-pruned Dijkstra
// ---------------------------------------------------------------------------

/// The §8 trap fixture: v holds two Pareto-incomparable states, (cost 2, 2
/// hops) and (cost 3, 1 hop). u's only ≤2-hop path runs through the
/// dominated-on-cost state (s→v 3.0, then v→u). Depth-pruned single-label
/// Dijkstra keeps only v=(2.0, 2 hops), prunes v→u at hop 3, and loses u
/// entirely.
const DEPTH_TRAP: &str = "e[f, t, w] <- [['s','a',1.0],['a','v',1.0],['s','v',3.0],\
                          ['v','u',1.0]]\ns[n] <- [['s']]\n";

#[test]
fn g1_depth_trap_layered_labels_not_depth_pruning() {
    let (_dir, db) = open_db();
    let res = run(
        &db,
        &format!(
            "{DEPTH_TRAP}?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], max_nodes: 10, max_depth: 2)"
        ),
    );
    assert_eq!(
        res.rows,
        vec![
            row("a", 1.0, Some("s"), 1),
            row("s", 0.0, None, 0),
            row("u", 4.0, Some("v"), 2),
            row("v", 2.0, Some("a"), 2),
        ]
    );
}

/// The §8 "never re-count" discriminator (added after the pre-PR adversarial
/// review found the re-COUNT half of "later states … without emitting or
/// re-counting" unpinned): on the depth trap with `max_nodes: 4`, v settles
/// TWICE — state (2.0, 2 hops) first, then the Pareto-incomparable
/// (3.0, 1 hop) — before u's only depth-feasible admission (4.0 through v's
/// 1-hop state). An implementation that counts every settled state against
/// the budget (instead of first-per-node only) exhausts the budget on v's
/// second settle and silently drops u.
#[test]
fn g4_second_settle_of_admitted_node_spends_no_budget() {
    let (_dir, db) = open_db();
    let res = run(
        &db,
        &format!(
            "{DEPTH_TRAP}?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], max_nodes: 4, max_depth: 2)"
        ),
    );
    assert_eq!(
        res.rows,
        vec![
            row("a", 1.0, Some("s"), 1),
            row("s", 0.0, None, 0),
            row("u", 4.0, Some("v"), 2),
            row("v", 2.0, Some("a"), 2),
        ]
    );
}

#[test]
fn g2_unweighted_depth_agrees_with_datalog_hop_oracle() {
    let (_dir, db) = open_db();
    run(&db, ":create e {f: String, t: String => w: Float}");
    run(
        &db,
        "?[f, t, w] <- [['n0','n1',1.0],['n0','n2',1.0],['n0','n7',1.0],['n1','n3',1.0],\
         ['n2','n3',1.0],['n2','n8',1.0],['n3','n4',1.0],['n4','n5',1.0],['n5','n6',1.0],\
         ['n6','n0',1.0],['n7','n8',1.0],['n8','n9',1.0],['n9','n1',1.0]] :put e {f, t => w}",
    );
    // Independent oracle: recursive Datalog min-hop distance, capped at 3.
    let oracle = run(
        &db,
        "start[n] <- [['n0']]\nr[n, d] := start[n], d = 0\n\
         r[t, d1] := r[f, d], *e[f, t, w], d < 3, d1 = d + 1\n?[n, min(d)] := r[n, d]",
    );
    let mut expect: BTreeMap<String, i64> = BTreeMap::new();
    for r in &oracle.rows {
        let DataValue::Str(n) = &r[0] else { panic!() };
        expect.insert(n.to_string(), r[1].get_int().unwrap());
    }
    let got = run(
        &db,
        "start[n] <- [['n0']]\n?[n, c, p, d] <~ BudgetedTraversal(*e[f, t, w], start[n], \
         max_nodes: 100, max_depth: 3)",
    );
    assert_eq!(got.rows.len(), expect.len(), "node sets differ");
    for r in &got.rows {
        let DataValue::Str(n) = &r[0] else { panic!() };
        let d = *expect
            .get(n.as_str())
            .unwrap_or_else(|| panic!("engine admitted {n} outside the oracle set"));
        // Unweighted collapse: cost ≡ hops, and depth is the min hop count.
        assert_eq!(r[1].get_float().unwrap(), d as f64, "cost of {n}");
        assert_eq!(r[3].get_int().unwrap(), d, "depth of {n}");
    }
}

#[test]
fn g3_nonbinding_max_depth_byte_equals_unset() {
    let (_dir, db) = open_db();
    // Strictly positive weights: A1's full-row byte-equality claim applies.
    let unset = run(
        &db,
        &format!(
            "{DIAMOND_TAIL}?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], max_nodes: 10)"
        ),
    );
    let nonbinding = run(
        &db,
        &format!(
            "{DIAMOND_TAIL}?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], max_nodes: 10, max_depth: 50)"
        ),
    );
    assert_eq!(unset.rows, nonbinding.rows);
    assert_eq!(unset.rows, diamond_tail_expected());
}

// ---------------------------------------------------------------------------
// (h) zero-weight edges: legal, terminating, witness-deterministic
// ---------------------------------------------------------------------------

#[test]
fn h1_zero_weight_acyclic_witness_deterministic() {
    let (_dir, db) = open_db();
    let script = "e[f, t, w] <- [['s','a',0.0],['s','b',0.0],['a','t',0.0],['b','t',0.0]]\n\
                  s[n] <- [['s']]\n\
                  ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], max_nodes: 10)";
    let expected = vec![
        row("a", 0.0, Some("s"), 1),
        row("b", 0.0, Some("s"), 1),
        row("s", 0.0, None, 0),
        row("t", 0.0, Some("a"), 2),
    ];
    let first = run(&db, script);
    assert_eq!(first.rows, expected);
    for _ in 0..2 {
        assert_eq!(
            run(&db, script).rows,
            first.rows,
            "byte-stability across runs"
        );
    }
}

#[test]
fn h2_zero_weight_cycle_terminates() {
    let (_dir, db) = open_db();
    let res = run(
        &db,
        "e[f, t, w] <- [['s','a',0.0],['a','b',0.0],['b','a',0.0],['b','c',0.0]]\n\
         s[n] <- [['s']]\n\
         ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], max_nodes: 10)",
    );
    assert_eq!(
        res.rows,
        vec![
            row("a", 0.0, Some("s"), 1),
            row("b", 0.0, Some("a"), 2),
            row("c", 0.0, Some("b"), 3),
            row("s", 0.0, None, 0),
        ]
    );
}

/// Spec §3.3's settle-order reference, restated as literally as the spec
/// words it: each round admits the (cost, node)-least admissible node whose
/// min-cost path uses only already-settled intermediates — realized as seed
/// arcs plus one-edge extensions of the settled set (any longer
/// settled-interior path decomposes into these, so round-minimum =
/// spec-minimum). Deliberately naive and shared with NO engine code; its twin
/// lives in the mindgraph-rs oracle (duplication is intentional — sharing it
/// would un-independent the check).
fn settle_order_reference(
    edges: &[(&str, &str, f64)],
    seeds: &[(&str, f64)],
    budget: usize,
) -> Vec<(String, f64)> {
    let mut settled: Vec<(String, f64)> = Vec::new();
    fn consider(cost: f64, v: &str, best: &mut Option<(f64, String)>, settled: &[(String, f64)]) {
        if settled.iter().any(|(u, _)| u == v) {
            return;
        }
        let better = match best {
            None => true,
            Some((bc, bv)) => {
                cost.total_cmp(bc) == std::cmp::Ordering::Less
                    || (cost.total_cmp(bc) == std::cmp::Ordering::Equal && v < bv.as_str())
            }
        };
        if better {
            *best = Some((cost, v.to_string()));
        }
    }
    while settled.len() < budget {
        let mut best: Option<(f64, String)> = None;
        for (v, ic) in seeds {
            consider(*ic, v, &mut best, &settled);
        }
        for (u, ucost) in settled.clone() {
            for (f, t, c) in edges {
                if *f == u {
                    consider(ucost + c, t, &mut best, &settled);
                }
            }
        }
        match best {
            Some((c, v)) => settled.push((v, c)),
            None => break,
        }
    }
    settled
}

#[test]
fn h3_zero_weight_chain_budget_boundary_settle_order_not_sorted_prefix() {
    let (_dir, db) = open_db();
    // Names deliberately anti-alphabetical: a plain (cost, node)-sorted
    // prefix of the reachable set at budget 2 would be {a, b}; the §3.3
    // settle order is s, c, b, a.
    let res = run(
        &db,
        "e[f, t, w] <- [['s','c',0.0],['c','b',0.0],['b','a',0.0]]\ns[n] <- [['s']]\n\
         ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], max_nodes: 2)",
    );
    assert_eq!(
        res.rows,
        vec![row("c", 0.0, Some("s"), 1), row("s", 0.0, None, 0)]
    );

    // Independent reference across every budget through exhaustion.
    let edges = [("s", "c", 0.0), ("c", "b", 0.0), ("b", "a", 0.0)];
    for budget in 1..=5usize {
        let expect: BTreeSet<String> = settle_order_reference(&edges, &[("s", 0.0)], budget)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        let res = run(
            &db,
            &format!(
                "e[f, t, w] <- [['s','c',0.0],['c','b',0.0],['b','a',0.0]]\ns[n] <- [['s']]\n\
                 ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], max_nodes: {budget})"
            ),
        );
        assert_eq!(node_set(&res), expect, "budget {budget}");
    }
}

// ---------------------------------------------------------------------------
// (w) witness fixtures — amendment A2's discriminators for settle-from-stored
// and layered keep-first
// ---------------------------------------------------------------------------

#[test]
fn w1_equal_cost_parent_improving_second_arrival_single() {
    let (_dir, db) = open_db();
    // t's label arrives first as (3.0, 2, "z") via z, then improves
    // witness-only to (3.0, 2, "a") at a's settle. No push accompanies the
    // improvement (equal cost), so the queued entry must deliver the STORED
    // parent "a" — settling from the popped payload would emit "z".
    let res = run(
        &db,
        "e[f, t, w] <- [['s','z',1.0],['s','a',2.0],['z','t',2.0],['a','t',1.0]]\n\
         s[n] <- [['s']]\n\
         ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], max_nodes: 10)",
    );
    assert_eq!(
        res.rows,
        vec![
            row("a", 2.0, Some("s"), 1),
            row("s", 0.0, None, 0),
            row("t", 3.0, Some("a"), 2),
            row("z", 1.0, Some("s"), 1),
        ]
    );
}

#[test]
fn w2_equal_cost_hops_improving_second_arrival_single() {
    let (_dir, db) = open_db();
    // t first labels (3.0, 3 hops, "q"), then improves to (3.0, 2, "r") when
    // r settles. The queued (3.0, t, 3) entry must settle the stored label:
    // depth 2, parent "r" — settling from the popped key would emit depth 3.
    let res = run(
        &db,
        "e[f, t, w] <- [['s','p',1.0],['p','q',1.0],['q','t',1.0],['s','r',2.0],['r','t',1.0]]\n\
         s[n] <- [['s']]\n\
         ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], max_nodes: 10)",
    );
    assert_eq!(
        res.rows,
        vec![
            row("p", 1.0, Some("s"), 1),
            row("q", 2.0, Some("p"), 2),
            row("r", 2.0, Some("s"), 1),
            row("s", 0.0, None, 0),
            row("t", 3.0, Some("r"), 2),
        ]
    );
}

#[test]
fn w3_equal_cost_witness_improvement_layered() {
    let (_dir, db) = open_db();
    // W1 under max_depth: the same equal-cost witness improvement happens
    // WITHIN t's (hops 2) state. Keep-first would emit parent "z".
    let res = run(
        &db,
        "e[f, t, w] <- [['s','z',1.0],['s','a',2.0],['z','t',2.0],['a','t',1.0]]\n\
         s[n] <- [['s']]\n\
         ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], max_nodes: 10, max_depth: 2)",
    );
    assert_eq!(
        res.rows,
        vec![
            row("a", 2.0, Some("s"), 1),
            row("s", 0.0, None, 0),
            row("t", 3.0, Some("a"), 2),
            row("z", 1.0, Some("s"), 1),
        ]
    );
}

// ---------------------------------------------------------------------------
// (i) loud errors
// ---------------------------------------------------------------------------

#[test]
fn i_loud_errors() {
    let (_dir, db) = open_db();

    // Negative weight, positional strict scan.
    let m = err(
        &db,
        "e[f, t, w] <- [['a','b',-1.0]]\ns[n] <- [['a']]\n\
         ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], max_nodes: 5)",
    );
    assert!(m.contains("edge weight"), "{m}");

    // Negative weight, cached arm: the projection builds permissively and the
    // strict consumer rejects at consume time.
    run(&db, ":create eneg {f: String, t: String => w: Float}");
    run(&db, "?[f, t, w] <- [['a','b',-1.0]] :put eneg {f, t => w}");
    run(&db, "::graph create gneg {edges: eneg}");
    let m = err(
        &db,
        "s[n] <- [['a']]\n?[n, c, p, d] <~ BudgetedTraversal(s[n], graph: 'gneg', max_nodes: 5)",
    );
    assert!(m.contains("negative edge weight"), "{m}");

    // NaN / +inf weights, injected via params (no NaN literal exists in-script).
    for bad in [f64::NAN, f64::INFINITY] {
        let m = err_with(
            &db,
            "e[f, t, w] <- [['a','b',$w]]\ns[n] <- [['a']]\n\
             ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], max_nodes: 5)",
            BTreeMap::from([("w".to_string(), DataValue::from(bad))]),
        );
        assert!(m.contains("edge weight"), "weight {bad}: {m}");
    }

    // Bad seed initial_cost: negative, then non-numeric.
    for seeds in ["[['s', -1.0]]", "[['s', 'oops']]"] {
        let m = err(
            &db,
            &format!(
                "e[f, t, w] <- [['s','a',1.0]]\ns[n, c] <- {seeds}\n\
                 ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n, c], max_nodes: 5)"
            ),
        );
        assert!(m.contains("initial_cost"), "seeds {seeds}: {m}");
    }
    // NaN / ±inf seed initial_cost, injected via params (the spec's finite-
    // only rule; a NaN cost admitted as a heap key would poison total_cmp).
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let m = err_with(
            &db,
            "e[f, t, w] <- [['s','a',1.0]]\ns[n, c] <- [['s', $c]]\n\
             ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n, c], max_nodes: 5)",
            BTreeMap::from([("c".to_string(), DataValue::from(bad))]),
        );
        assert!(m.contains("initial_cost"), "seed cost {bad}: {m}");
    }

    // Missing max_nodes.
    let m = err(
        &db,
        "e[f, t, w] <- [['a','b',1.0]]\ns[n] <- [['a']]\n\
         ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n])",
    );
    assert!(m.contains("max_nodes"), "{m}");

    // max_nodes: 0.
    let m = err(
        &db,
        "e[f, t, w] <- [['a','b',1.0]]\ns[n] <- [['a']]\n\
         ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], max_nodes: 0)",
    );
    assert!(m.contains("positive"), "{m}");

    // max_depth: 0.
    let m = err(
        &db,
        "e[f, t, w] <- [['a','b',1.0]]\ns[n] <- [['a']]\n\
         ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], max_nodes: 5, max_depth: 0)",
    );
    assert!(m.contains("positive"), "{m}");

    // max_cost domain (amendment A3): negative, NaN, +inf are all loud —
    // "unbounded" is spelled exactly one way (omit the option).
    let m = err(
        &db,
        "e[f, t, w] <- [['a','b',1.0]]\ns[n] <- [['a']]\n\
         ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], max_nodes: 5, max_cost: -1.0)",
    );
    assert!(m.contains("max_cost") && m.contains("finite"), "{m}");
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let m = err_with(
            &db,
            "e[f, t, w] <- [['a','b',1.0]]\ns[n] <- [['a']]\n\
             ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], max_nodes: 5, max_cost: $mc)",
            BTreeMap::from([("mc".to_string(), DataValue::from(bad))]),
        );
        assert!(
            m.contains("max_cost") && m.contains("finite"),
            "max_cost {bad}: {m}"
        );
    }

    // Over-bound gate: more bindings than the gate relation has columns is
    // rejected at input validation (spec §3.1's arity >= binding count),
    // whether or not admit: reads the overflow binding — never a
    // mid-traversal "tuple too short" or a silent run.
    run(&db, ":create gnarrow {uid: String => ok: Int}");
    run(&db, "?[uid, ok] <- [['a',1]] :put gnarrow {uid => ok}");
    for admit in ["extra == 1", "ok == 1"] {
        let m = err(
            &db,
            &format!(
                "e[f, t, w] <- [['a','b',1.0]]\ns[n] <- [['a']]\n\
                 ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], *gnarrow[uid, ok, extra], \
                 max_nodes: 5, admit: {admit})"
            ),
        );
        assert!(m.contains("arity"), "over-bound gate (admit: {admit}): {m}");
    }

    // admit: without a gate input is a bug, not a no-op.
    let m = err(
        &db,
        "e[f, t, w] <- [['a','b',1.0]]\ns[n] <- [['a']]\n\
         ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], max_nodes: 5, admit: ok == 1)",
    );
    assert!(m.contains("admit"), "{m}");
}

// ---------------------------------------------------------------------------
// (j) gate semantics
// ---------------------------------------------------------------------------

#[test]
fn j1_gate_zero_budget_starvation() {
    let (_dir, db) = open_db();
    run(&db, ":create g {uid: String => ok: Int}");
    run(
        &db,
        "?[uid, ok] <- [['a',1],['live',1],['dead',0]] :put g {uid => ok}",
    );
    // The inadmissible node holds the CHEAP edge: probing the gate only at
    // admission time would let `dead` consume the second budget slot.
    let res = run(
        &db,
        "e[f, t, w] <- [['a','dead',0.25],['a','live',5.0]]\ns[n] <- [['a']]\n\
         ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], *g[uid, ok], max_nodes: 2, admit: ok == 1)",
    );
    assert_eq!(
        res.rows,
        vec![row("a", 0.0, None, 0), row("live", 5.0, Some("a"), 1)]
    );
}

// The named `*gate{col}` fixed-rule input form: parsing it used to PANIC
// unconditionally (`strip_prefix(':')` on a `*`-prefixed `relation_ident` —
// inherited upstream bug, present at fork-base), which is why no test in the
// tree exercised it. It now parses, and binds by NAME in schema order (keys
// then non-keys), so brace order cannot misbind.
#[test]
fn j1b_named_gate_binding_parses_and_binds_by_name() {
    let (_dir, db) = open_db();
    run(&db, ":create g {uid: String => ok: Int}");
    run(
        &db,
        "?[uid, ok] <- [['a',1],['t',0],['b',1]] :put g {uid => ok}",
    );
    for gate in ["*g{uid, ok}", "*g{ok, uid}"] {
        let res = run(
            &db,
            &format!(
                "e[f, t, w] <- [['a','t',1.0],['a','b',1.0]]\ns[n] <- [['a']]\n\
                 ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], {gate}, \
                 max_nodes: 10, admit: ok == 1)"
            ),
        );
        assert_eq!(
            res.rows,
            vec![row("a", 0.0, None, 0), row("b", 1.0, Some("a"), 1)],
            "gate form {gate} misbound"
        );
    }
    // A misspelled braced name is the loud schema error, not a silent misbind.
    let m = err(
        &db,
        "e[f, t, w] <- [['a','b',1.0]]\ns[n] <- [['a']]\n\
         ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], *g{nosuch}, max_nodes: 5)",
    );
    assert!(m.contains("nosuch"), "{m}");
}

#[test]
fn j2_gate_not_a_bridge() {
    let (_dir, db) = open_db();
    run(&db, ":create g {uid: String => ok: Int}");
    run(
        &db,
        "?[uid, ok] <- [['a',1],['t',0],['b',1]] :put g {uid => ok}",
    );
    // t is gated out, so it never relays: b is admissible but unreachable.
    let res = run(
        &db,
        "e[f, t, w] <- [['a','t',1.0],['t','b',1.0]]\ns[n] <- [['a']]\n\
         ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], *g[uid, ok], max_nodes: 10, admit: ok == 1)",
    );
    assert_eq!(res.rows, vec![row("a", 0.0, None, 0)]);
}

#[test]
fn j3_gate_applies_to_seeds() {
    let (_dir, db) = open_db();
    run(&db, ":create g {uid: String => ok: Int}");
    run(
        &db,
        "?[uid, ok] <- [['s_dead',0],['s_live',1],['d1',1],['l1',1]] :put g {uid => ok}",
    );
    // The gated-out seed spends nothing and expands nothing: d1 (reachable
    // only through s_dead, and itself admissible) must be absent.
    let res = run(
        &db,
        "e[f, t, w] <- [['s_dead','d1',1.0],['s_live','l1',1.0]]\ns[n] <- [['s_dead'],['s_live']]\n\
         ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], *g[uid, ok], max_nodes: 10, admit: ok == 1)",
    );
    assert_eq!(
        res.rows,
        vec![
            row("l1", 1.0, Some("s_live"), 1),
            row("s_live", 0.0, None, 0)
        ]
    );
}

#[test]
fn j4_dangling_target_fails_bare_presence() {
    let (_dir, db) = open_db();
    run(&db, ":create g {uid: String => ok: Int}");
    run(&db, "?[uid, ok] <- [['a',1]] :put g {uid => ok}");
    // No admit: — bare presence. ghost has no gate row, so it is inadmissible.
    let res = run(
        &db,
        "e[f, t, w] <- [['a','ghost',1.0]]\ns[n] <- [['a']]\n\
         ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], *g[uid, ok], max_nodes: 10)",
    );
    assert_eq!(res.rows, vec![row("a", 0.0, None, 0)]);
}

#[test]
fn j5_admit_over_second_value_column() {
    let (_dir, db) = open_db();
    run(&db, ":create g3 {uid: String => flag: Int, ok: Int}");
    // Decoy: t's flag (position 1) is 1 — a predicate mis-indexed one column
    // left would admit t and reject a.
    run(
        &db,
        "?[uid, flag, ok] <- [['a',0,1],['t',1,0]] :put g3 {uid => flag, ok}",
    );
    let res = run(
        &db,
        "e[f, t, w] <- [['a','t',1.0]]\ns[n] <- [['a']]\n\
         ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], *g3[uid, flag, ok], max_nodes: 10, admit: ok == 1)",
    );
    assert_eq!(res.rows, vec![row("a", 0.0, None, 0)]);
}

#[test]
fn j6_positional_prefix_misbinding_silently_empty() {
    let (_dir, db) = open_db();
    run(&db, ":create g4 {uid: String => created_at: Int, ok: Int}");
    run(
        &db,
        "?[uid, created_at, ok] <- [['a',7,1]] :put g4 {uid => created_at, ok}",
    );
    // The documented §3.1 footgun: `x` binds POSITIONALLY to column 1
    // (created_at, never 1), not to `ok` — every node is silently
    // inadmissible, seeds included, and the output is empty.
    let res = run(
        &db,
        "e[f, t, w] <- [['a','b',1.0]]\ns[n] <- [['a']]\n\
         ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], *g4[uid, x], max_nodes: 10, admit: x == 1)",
    );
    assert!(res.rows.is_empty(), "{:?}", res.rows);
}

#[test]
fn j7_int_float_seam() {
    let (_dir, db) = open_db();
    // (a) Gate keyed Float, candidate node Int: the gate probe is a
    // STRUCTURAL prefix match, so Int 1 misses the Float 1.0 row — everything
    // (seed included) is gated out.
    run(&db, ":create gf {uid: Float => ok: Int}");
    run(&db, "?[uid, ok] <- [[1.0,1],[2.0,1]] :put gf {uid => ok}");
    let res = run(
        &db,
        "e[f, t, w] <- [[1, 2, 1.0]]\ns[n] <- [[1]]\n\
         ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], *gf[uid, ok], max_nodes: 10, admit: ok == 1)",
    );
    assert!(res.rows.is_empty(), "{:?}", res.rows);

    // (b) Gate keyed Int with ok stored as Float: `==` INSIDE admit is
    // numeric, so Float 1.0 == 1 admits.
    run(&db, ":create gi {uid: Int => ok: Float}");
    run(&db, "?[uid, ok] <- [[1,1.0],[2,1.0]] :put gi {uid => ok}");
    let res = run(
        &db,
        "e[f, t, w] <- [[1, 2, 1.0]]\ns[n] <- [[1]]\n\
         ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], *gi[uid, ok], max_nodes: 10, admit: ok == 1)",
    );
    assert_eq!(
        res.rows,
        vec![
            vec![
                DataValue::from(1i64),
                DataValue::from(0.0),
                DataValue::Null,
                DataValue::from(0i64),
            ],
            vec![
                DataValue::from(2i64),
                DataValue::from(1.0),
                DataValue::from(1i64),
                DataValue::from(1i64),
            ],
        ]
    );
}

// ---------------------------------------------------------------------------
// (k) seed semantics
// ---------------------------------------------------------------------------

#[test]
fn k1_duplicate_seeds_min_merge() {
    let (_dir, db) = open_db();
    let res = run(
        &db,
        "e[f, t, w] <- [['s','a',1.0]]\ns[n, c] <- [['s', 5.0], ['s', 1.0]]\n\
         ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n, c], max_nodes: 10)",
    );
    assert_eq!(
        res.rows,
        vec![row("a", 2.0, Some("s"), 1), row("s", 1.0, None, 0)]
    );
}

#[test]
fn k2_csr_absent_seed_emits_as_loose_root() {
    let (_dir, db) = open_db();
    let res = run(
        &db,
        "e[f, t, w] <- [['x','y',1.0]]\ns[n] <- [['iso']]\n\
         ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], max_nodes: 10)",
    );
    assert_eq!(res.rows, vec![row("iso", 0.0, None, 0)]);
}

#[test]
fn k3_seed_reachable_below_own_initial_cost() {
    let (_dir, db) = open_db();
    // b is seeded at 5.0 but reachable from a at 1.0: the path label wins
    // over the root form.
    let res = run(
        &db,
        "e[f, t, w] <- [['a','b',1.0]]\ns[n, c] <- [['a', 0.0], ['b', 5.0]]\n\
         ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n, c], max_nodes: 10)",
    );
    assert_eq!(
        res.rows,
        vec![row("a", 0.0, None, 0), row("b", 1.0, Some("a"), 1)]
    );
}

#[test]
fn k4_initial_cost_biases_admission_order() {
    let (_dir, db) = open_db();
    // z (cost 1.0 through x) outranks the expensive seed y (3.0) at the cut.
    let res = run(
        &db,
        "e[f, t, w] <- [['x','z',1.0]]\ns[n, c] <- [['x', 0.0], ['y', 3.0]]\n\
         ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n, c], max_nodes: 2)",
    );
    assert_eq!(
        res.rows,
        vec![row("x", 0.0, None, 0), row("z", 1.0, Some("x"), 1)]
    );
}

#[test]
fn k5_int_seed_vs_float_csr_ids_is_structural() {
    let (_dir, db) = open_db();
    // CSR ids are Float 1.0 / Float 2.0; the Int 1 seed is a structural
    // non-match, so it emits as a loose root and 2.0 stays unreached.
    let res = run(
        &db,
        "e[f, t, w] <- [[1.0, 2.0, 1.0]]\ns[n] <- [[1]]\n\
         ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], max_nodes: 10)",
    );
    assert_eq!(
        res.rows,
        vec![vec![
            DataValue::from(1i64),
            DataValue::from(0.0),
            DataValue::Null,
            DataValue::from(0i64),
        ]]
    );
}

#[test]
fn k6_wide_seed_relation_reads_col1_drops_rest() {
    let (_dir, db) = open_db();
    let res = run(
        &db,
        "e[f, t, w] <- [['s','a',1.0]]\ns[n, c, j, k] <- [['s', 2.0, 'junk', 7]]\n\
         ?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n, c, j, k], max_nodes: 10)",
    );
    assert_eq!(
        res.rows,
        vec![row("a", 3.0, Some("s"), 1), row("s", 2.0, None, 0)]
    );
}

// ---------------------------------------------------------------------------
// (l) empty graph
// ---------------------------------------------------------------------------

#[test]
fn l_empty_graph_seeds_still_emit() {
    let (_dir, db) = open_db();
    run(&db, ":create ee {f: String, t: String => w: Float}");
    // No early return on an empty CSR: seeds classify as loose and emit.
    let res = run(
        &db,
        "s[n] <- [['s1'],['s2']]\n?[n, c, p, d] <~ BudgetedTraversal(*ee[f, t, w], s[n], max_nodes: 10)",
    );
    assert_eq!(
        res.rows,
        vec![row("s1", 0.0, None, 0), row("s2", 0.0, None, 0)]
    );

    // Same with a gate: the loose-seed path still respects admissibility.
    run(&db, ":create gl {uid: String}");
    run(&db, "?[uid] <- [['s1']] :put gl {uid}");
    let res = run(
        &db,
        "s[n] <- [['s1'],['s2']]\n?[n, c, p, d] <~ BudgetedTraversal(*ee[f, t, w], s[n], *gl[uid], max_nodes: 10)",
    );
    assert_eq!(res.rows, vec![row("s1", 0.0, None, 0)]);
}

// ---------------------------------------------------------------------------
// (m) combined bounds intersect
// ---------------------------------------------------------------------------

#[test]
fn m_combined_bounds_intersect() {
    let (_dir, db) = open_db();
    // Neither bound alone produces the combined set, so the test
    // discriminates true intersection:
    //   depth only:  {s, a, v, u@(4.0, 2 hops)}
    //   cost only:   {s, a, v, u@(3.0, 3 hops)} — the cheap deep path
    //   both:        {s, a, v} — u's ≤2-hop path costs 4.0 > 3.5
    let combined = run(
        &db,
        &format!(
            "{DEPTH_TRAP}?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], max_nodes: 10, \
             max_depth: 2, max_cost: 3.5)"
        ),
    );
    assert_eq!(
        combined.rows,
        vec![
            row("a", 1.0, Some("s"), 1),
            row("s", 0.0, None, 0),
            row("v", 2.0, Some("a"), 2),
        ]
    );
    let depth_only = run(
        &db,
        &format!(
            "{DEPTH_TRAP}?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], max_nodes: 10, max_depth: 2)"
        ),
    );
    assert_eq!(
        depth_only.rows,
        vec![
            row("a", 1.0, Some("s"), 1),
            row("s", 0.0, None, 0),
            row("u", 4.0, Some("v"), 2),
            row("v", 2.0, Some("a"), 2),
        ]
    );
    let cost_only = run(
        &db,
        &format!(
            "{DEPTH_TRAP}?[n, c, p, d] <~ BudgetedTraversal(e[f, t, w], s[n], max_nodes: 10, max_cost: 3.5)"
        ),
    );
    assert_eq!(
        cost_only.rows,
        vec![
            row("a", 1.0, Some("s"), 1),
            row("s", 0.0, None, 0),
            row("u", 3.0, Some("v"), 3),
            row("v", 2.0, Some("a"), 2),
        ]
    );
}

// ---------------------------------------------------------------------------
// (n) poison: :timeout aborts the expansion loop
// ---------------------------------------------------------------------------

/// A cost/hops-antichain line graph that explodes only the layered state
/// space: reaching position p with k long (i→i+2) hops costs 0.4p + 0.2k at
/// p − k hops — strictly Pareto-incomparable in k, so ~p/2 states survive per
/// node (~2.25M states, ~4.5M relaxations): tens of seconds in a debug build.
/// The CSR build is tiny (6k edges), so an abort is attributable to the
/// expansion loop's poison mask, not the already-shipped build mask.
#[test]
fn n_timeout_aborts_layered_expansion() {
    let (_dir, db) = open_db();
    run(&db, ":create e {f: Int, t: Int => w: Float}");
    let mut edges = vec![];
    for i in 0..3000i64 {
        edges.push(vec![
            DataValue::from(i),
            DataValue::from(i + 1),
            DataValue::from(0.4),
        ]);
        edges.push(vec![
            DataValue::from(i),
            DataValue::from(i + 2),
            DataValue::from(1.0),
        ]);
    }
    let mut data = BTreeMap::new();
    data.insert(
        "e".to_string(),
        NamedRows::new(
            vec!["f".to_string(), "t".to_string(), "w".to_string()],
            edges,
        ),
    );
    db.import_relations(data).unwrap();

    let script = "s[n] <- [[0]]\n?[n, c, p, d] <~ BudgetedTraversal(*e[f, t, w], s[n], \
                  max_nodes: 100000, max_depth: 3000) :timeout 1";
    let t0 = Instant::now();
    let res = db.run_script(script, BTreeMap::new(), ScriptMutability::Immutable);
    let elapsed = t0.elapsed();
    assert!(
        res.is_err(),
        "`:timeout 1` on the state-explosion fixture should error, got Ok"
    );
    assert!(
        elapsed < Duration::from_secs(6),
        "aborted after {elapsed:?}; the expansion loop is not checking poison"
    );
}

// ---------------------------------------------------------------------------
// (o) read-only: runs under an Immutable script
// ---------------------------------------------------------------------------

#[test]
fn o_runs_under_immutable_script() {
    let (_dir, db) = open_db();
    run(&db, ":create e {f: String, t: String => w: Float}");
    run(
        &db,
        "?[f, t, w] <- [['a','b',1.0],['a','c',4.0],['b','c',1.0],['b','d',5.0],['c','d',1.0]] \
         :put e {f, t => w}",
    );
    run(&db, "::graph create og {edges: e}");

    let immutable = |s: &str| {
        db.run_script(s, BTreeMap::new(), ScriptMutability::Immutable)
            .unwrap_or_else(|e| panic!("immutable script failed: {s}\n{e:?}"))
    };
    let pos = immutable(
        "s[n] <- [['a']]\n?[n, c, p, d] <~ BudgetedTraversal(*e[f, t, w], s[n], max_nodes: 10)",
    );
    assert_eq!(pos.rows, diamond_tail_expected());
    let proj = immutable(
        "s[n] <- [['a']]\n?[n, c, p, d] <~ BudgetedTraversal(s[n], graph: 'og', max_nodes: 10)",
    );
    assert_eq!(proj.rows, diamond_tail_expected());
}

// ---------------------------------------------------------------------------
// Out-of-tree `supports_projection` (amendment A5's replacement for the
// prototype's incidental coverage): the parse guard consults the REGISTERED
// impl's own method.
// ---------------------------------------------------------------------------

struct ProjProbe;

impl FixedRule for ProjProbe {
    fn arity(
        &self,
        _options: &BTreeMap<smartstring::alias::String, Expr>,
        _rule_head: &[Symbol],
        _span: SourceSpan,
    ) -> miette::Result<usize> {
        Ok(1)
    }
    fn supports_projection(&self) -> bool {
        true
    }
    fn run(
        &self,
        payload: FixedRulePayload<'_, '_>,
        out: &mut RegularTempStore,
        poison: Poison,
    ) -> miette::Result<()> {
        let (source, _base) =
            payload.graph_input(0, VariantSpec::weighted(false, true), &poison)?;
        for dv in source.indices() {
            out.put(vec![dv.clone()]);
        }
        Ok(())
    }
}

/// Deliberately does NOT override `supports_projection` — the negative control.
struct NoProjProbe;

impl FixedRule for NoProjProbe {
    fn arity(
        &self,
        _options: &BTreeMap<smartstring::alias::String, Expr>,
        _rule_head: &[Symbol],
        _span: SourceSpan,
    ) -> miette::Result<usize> {
        Ok(1)
    }
    fn run(
        &self,
        _payload: FixedRulePayload<'_, '_>,
        _out: &mut RegularTempStore,
        _poison: Poison,
    ) -> miette::Result<()> {
        Ok(())
    }
}

#[test]
fn registered_rule_supports_projection_unlocks_graph_option() {
    let (_dir, db) = open_db();
    db.register_fixed_rule("ProjProbe".to_string(), ProjProbe)
        .unwrap();
    run(&db, ":create pe {f: String, t: String => w: Float}");
    run(
        &db,
        "?[f, t, w] <- [['a','b',1.0],['b','c',2.0]] :put pe {f, t => w}",
    );
    run(&db, "::graph create pg {edges: pe}");

    // The parse guard (parse/query.rs) must consult the registered impl's own
    // supports_projection — no shipped rule exercised this path before.
    let via_graph = run(&db, "?[n] <~ ProjProbe(graph: 'pg')");
    let positional = run(&db, "?[n] <~ ProjProbe(*pe[f, t, w])");
    assert_eq!(via_graph.rows, positional.rows);
    assert_eq!(via_graph.rows.len(), 3);
}

#[test]
fn registered_rule_without_projection_support_rejected() {
    let (_dir, db) = open_db();
    db.register_fixed_rule("NoProjProbe".to_string(), NoProjProbe)
        .unwrap();
    run(&db, ":create pe {f: String, t: String => w: Float}");
    run(&db, "?[f, t, w] <- [['a','b',1.0]] :put pe {f, t => w}");
    run(&db, "::graph create pg {edges: pe}");

    let m = err(&db, "?[n] <~ NoProjProbe(graph: 'pg')");
    assert!(m.contains("graph projection"), "{m}");
}
