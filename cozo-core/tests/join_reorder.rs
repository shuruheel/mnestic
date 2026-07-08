/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Tests for the deterministic, stat-free greedy join reorder (mnestic fork,
//! 0.10.5 — item H). The pass rewrites eligible positive conjunctions into a
//! min-new-vars order so an LLM-authored, naively-ordered triangle no longer
//! spins on an N^3 intermediate — WITHOUT changing any result.
//!
//! Planner assertions use the SQLite backend on a `tempfile::tempdir()` (the
//! `mem` backend uses a separate `mem_*` join operator and does not exercise the
//! `stored_*` path — see `matjoin_regression.rs`); correctness is asserted on
//! both backends.

use cozo::{DataValue, DbInstance, NamedRows, ScriptMutability};
use std::collections::BTreeMap;

fn run_mut(db: &DbInstance, s: &str) {
    db.run_script(s, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap();
}

fn run_p(db: &DbInstance, s: &str, params: BTreeMap<String, DataValue>) -> NamedRows {
    db.run_script(s, params, ScriptMutability::Immutable).unwrap()
}

fn run_mut_p(db: &DbInstance, s: &str, params: BTreeMap<String, DataValue>) {
    db.run_script(s, params, ScriptMutability::Mutable).unwrap();
}

fn run(db: &DbInstance, s: &str) -> NamedRows {
    run_p(db, s, BTreeMap::new())
}

fn sqlite_db(dir: &tempfile::TempDir) -> DbInstance {
    DbInstance::new(
        "sqlite",
        dir.path().join("jr.db").to_str().unwrap(),
        Default::default(),
    )
    .unwrap()
}

fn mem_db() -> DbInstance {
    DbInstance::new("mem", "", Default::default()).unwrap()
}

/// Build the members-first triangle fixture: `n_members` people all in group 0,
/// and `clique` people (0..clique) forming a complete directed `knows` clique.
/// The triangle query then returns `clique*(clique-1)*(clique-2)` ordered rows.
fn populate(db: &DbInstance, n_members: usize, clique: usize) {
    run_mut(db, ":create member { c: Int, p: Int }");
    run_mut(db, ":create knows { a: Int, b: Int }");

    let members: Vec<DataValue> = (0..n_members)
        .map(|p| DataValue::List(vec![DataValue::from(0i64), DataValue::from(p as i64)]))
        .collect();
    let mut mp = BTreeMap::new();
    mp.insert("rows".to_string(), DataValue::List(members));
    run_mut_p(db, "?[c, p] <- $rows :put member { c, p }", mp);

    let mut edges = vec![];
    for a in 0..clique {
        for b in 0..clique {
            if a != b {
                edges.push(DataValue::List(vec![
                    DataValue::from(a as i64),
                    DataValue::from(b as i64),
                ]));
            }
        }
    }
    let mut kp = BTreeMap::new();
    kp.insert("rows".to_string(), DataValue::List(edges));
    run_mut_p(db, "?[a, b] <- $rows :put knows { a, b }", kp);
}

/// The naive, LLM-style members-first triangle (all three `member` atoms before
/// any `knows` filter): the class that spins on the cubic intermediate.
const NAIVE: &str = "?[c, p1, p2, p3] := *member[c, p1], *member[c, p2], *member[c, p3], \
     *knows[p1, p2], *knows[p2, p3], *knows[p1, p3]";

/// The hand-tuned selective-first triangle (the `knows` clique first). This
/// order is already stepwise-greedy-consistent, so the pass is the identity.
const SELECTIVE: &str = "?[c, p1, p2, p3] := *knows[p1, p2], *knows[p2, p3], *knows[p1, p3], \
     *member[c, p1], *member[c, p2], *member[c, p3]";

fn explain(db: &DbInstance, query: &str) -> NamedRows {
    run(db, &format!("::explain {{ {query} }}"))
}

/// The ordered sequence of relation names loaded (`load_stored` rows), e.g.
/// `[":member", ":member", ":knows", …]`. This captures the join order.
fn load_refs(plan: &NamedRows) -> Vec<String> {
    plan.rows
        .iter()
        .filter(|r| r[4].get_str() == Some("load_stored"))
        .map(|r| r[5].get_str().unwrap_or("").to_string())
        .collect()
}

/// A full, comparable signature of the plan: (op, joins_on, out_bindings) per
/// atom. Byte-identical signatures mean byte-identical plans.
fn plan_sig(plan: &NamedRows) -> Vec<(String, String, String)> {
    plan.rows
        .iter()
        .map(|r| {
            (
                r[4].get_str().unwrap_or("").to_string(),
                format!("{:?}", r[6]),
                format!("{:?}", r[8]),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 1. Repro-class win: same count, and the plan no longer leads with the cubic
//    all-triples join.
// ---------------------------------------------------------------------------

fn assert_repro_win(db: &DbInstance, assert_plan: bool) {
    // ~120 members in one group; a 12-clique -> 12*11*10 = 1320 ordered triangles.
    populate(db, 120, 12);

    // Greedy (default) runs the query fast and returns the correct count.
    let greedy = run(db, NAIVE);
    assert_eq!(greedy.rows.len(), 1320, "greedy default must return 1320 rows");

    // Same count under the escape hatch (correctness parity). At N=120 the
    // written order is quadratic-per-group but still finite; the small-N parity
    // tests below cover the pathological class without executing it here.
    let written = run(db, &format!("{NAIVE} :reorder written"));
    assert_eq!(
        written.rows.len(),
        greedy.rows.len(),
        "reorder must not change the result count"
    );

    if assert_plan {
        // The greedy plan pulls a `knows` filter into the 3rd load, before the
        // 3rd `member`; the written plan keeps all three members first.
        let greedy_loads = load_refs(&explain(db, NAIVE));
        let written_loads = load_refs(&explain(db, &format!("{NAIVE} :reorder written")));
        assert_eq!(
            written_loads,
            vec![":member", ":member", ":member", ":knows", ":knows", ":knows"],
            "written order must keep the authored members-first plan"
        );
        assert_eq!(
            greedy_loads,
            vec![":member", ":member", ":knows", ":member", ":knows", ":knows"],
            "greedy must pull a knows filter forward (semi-join pushdown)"
        );
        assert_ne!(greedy_loads, written_loads, "the plan must have changed");
    }
}

#[test]
fn repro_triangle_win_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    assert_repro_win(&sqlite_db(&dir), true);
}

#[test]
fn repro_triangle_win_mem() {
    // Correctness only on mem (its join operator differs; no plan assertion).
    assert_repro_win(&mem_db(), false);
}

// ---------------------------------------------------------------------------
// 2. Escape hatch: `:reorder written` keeps the authored order.
// ---------------------------------------------------------------------------

#[test]
fn escape_hatch_keeps_written_order() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite_db(&dir);
    populate(&db, 12, 12);

    let default_sig = plan_sig(&explain(&db, NAIVE));
    let written_sig = plan_sig(&explain(&db, &format!("{NAIVE} :reorder written")));
    assert_ne!(default_sig, written_sig, "default should reorder the naive body");

    // `:reorder greedy` is the explicit default and matches the plain default.
    let greedy_sig = plan_sig(&explain(&db, &format!("{NAIVE} :reorder greedy")));
    assert_eq!(greedy_sig, default_sig, "`:reorder greedy` == default");

    // An unknown mode is a clear parse error.
    let err = db
        .run_script(
            &format!("::explain {{ {NAIVE} :reorder sideways }}"),
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .unwrap_err();
    assert!(
        format!("{err:?}").contains("reorder"),
        "unknown :reorder mode must error clearly, got: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 3. Identity guarantee: a greedy-consistent (hand-tuned) query is byte-
//    identical with and without the pass.
// ---------------------------------------------------------------------------

#[test]
fn identity_on_hand_tuned_order() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite_db(&dir);
    populate(&db, 12, 12);

    let default_sig = plan_sig(&explain(&db, SELECTIVE));
    let written_sig = plan_sig(&explain(&db, &format!("{SELECTIVE} :reorder written")));
    assert_eq!(
        default_sig, written_sig,
        "greedy must be the identity on a stepwise-greedy-consistent order"
    );
}

// ---------------------------------------------------------------------------
// 4. Pending leading unification: reorder must not introduce a compile error.
//
//    The verifier's fallback case is a leading unification whose binding source
//    lives in a later relation. With this reconstruction the trailing/leading
//    split keeps every such body well-orderable, so the pass reorders it AND it
//    still compiles + returns correct rows. (The catch-and-retry-written valve
//    in `convert_to_well_ordered_rule` is defensive insurance for future
//    reconstruction changes; it cannot be triggered by this shape.)
// ---------------------------------------------------------------------------

/// Eligible (4 stored relations) with a leading pending unification `y = a`
/// whose source `a` lives in `r1`, the relation greedy defers to last (it
/// introduces the most new variables). The unification stays bound-before-use in
/// both the written and greedy orders.
const PENDING_UNIF_QUERY: &str = "?[k, y] := y = a, \
     *r0[k, o], *r1[k, a, e, f], *r2[k, d], *r3[k, g]";

fn setup_pending(db: &DbInstance) {
    run_mut(db, ":create r0 { k: Int, o: Int }");
    run_mut(db, ":create r1 { k: Int, a: Int, e: Int, f: Int }");
    run_mut(db, ":create r2 { k: Int, d: Int }");
    run_mut(db, ":create r3 { k: Int, g: Int }");
    run_mut(db, "?[k, o] <- [[1, 10], [2, 20]] :put r0 { k, o }");
    run_mut(
        db,
        "?[k, a, e, f] <- [[1, 100, 0, 0], [2, 200, 0, 0]] :put r1 { k, a, e, f }",
    );
    run_mut(db, "?[k, d] <- [[1, 1], [2, 2]] :put r2 { k, d }");
    run_mut(db, "?[k, g] <- [[1, 7], [2, 8]] :put r3 { k, g }");
}

fn assert_pending_unif_parity(db: &DbInstance) {
    setup_pending(db);
    // Must compile and return rows with reorder ON (default).
    let default = run(db, PENDING_UNIF_QUERY);
    let written = run(db, &format!("{PENDING_UNIF_QUERY} :reorder written"));
    assert_eq!(written.rows.len(), 2, "written must return 2 rows");
    let mut a = default.rows.clone();
    let mut b = written.rows.clone();
    a.sort();
    b.sort();
    assert_eq!(a, b, "reorder must return the same rows as written order");
}

#[test]
fn pending_leading_unif_parity_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    assert_pending_unif_parity(&sqlite_db(&dir));
}

#[test]
fn pending_leading_unif_parity_mem() {
    assert_pending_unif_parity(&mem_db());
}

#[test]
fn pending_leading_unif_is_actually_reordered() {
    // Confirm the pass really does reorder this body (not a no-op) while keeping
    // the leading pending unification bound-before-use.
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite_db(&dir);
    setup_pending(&db);
    let default_loads = load_refs(&explain(&db, PENDING_UNIF_QUERY));
    let written_loads = load_refs(&explain(&db, &format!("{PENDING_UNIF_QUERY} :reorder written")));
    assert_eq!(written_loads, vec![":r0", ":r1", ":r2", ":r3"]);
    assert_eq!(
        default_loads,
        vec![":r0", ":r2", ":r3", ":r1"],
        "greedy defers the wide relation r1 past the narrow r2/r3"
    );
}

// ---------------------------------------------------------------------------
// 5. Cartesian step: a genuinely disconnected conjunction is annotated in the
//    plan (and warned about in logs).
// ---------------------------------------------------------------------------

#[test]
fn cartesian_step_annotated_in_explain() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite_db(&dir);
    run_mut(&db, ":create ra { x: Int, y: Int }");
    run_mut(&db, ":create rb { x: Int, y: Int }");
    run_mut(&db, ":create rc { x: Int, y: Int }");

    // No shared variables across the three atoms -> Cartesian product.
    let disconnected = "?[a, c, e] := *ra[a, b], *rb[c, d], *rc[e, f]";
    let plan = explain(&db, disconnected);
    let ops: Vec<String> = plan
        .rows
        .iter()
        .map(|r| r[4].get_str().unwrap_or("").to_string())
        .collect();
    assert!(
        ops.iter().any(|o| o.contains("(cartesian)")),
        "a disconnected conjunction must be annotated (cartesian), got: {ops:?}"
    );
}

// ---------------------------------------------------------------------------
// 6. Ordinary multi-atom queries: identical results with reorder ON vs written.
// ---------------------------------------------------------------------------

fn sorted_rows(mut nr: NamedRows) -> Vec<Vec<DataValue>> {
    nr.rows.sort();
    std::mem::take(&mut nr.rows)
}

#[test]
fn ordinary_queries_result_parity() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite_db(&dir);
    populate(&db, 30, 8);

    // A handful of differently-ordered conjunctions — all must agree with their
    // `:reorder written` counterpart, and all must agree with each other.
    let variants = [
        NAIVE,
        SELECTIVE,
        "?[c, p1, p2, p3] := *member[c, p1], *knows[p1, p2], *member[c, p2], \
             *knows[p2, p3], *member[c, p3], *knows[p1, p3]",
        // With an expression filter interleaved.
        "?[c, p1, p2, p3] := *member[c, p1], *member[c, p2], *member[c, p3], \
             *knows[p1, p2], *knows[p2, p3], *knows[p1, p3], p1 < p3",
    ];
    let mut baseline: Option<Vec<Vec<DataValue>>> = None;
    for v in variants {
        let on = sorted_rows(run(&db, v));
        let off = sorted_rows(run(&db, &format!("{v} :reorder written")));
        assert_eq!(on, off, "reorder ON must equal written for `{v}`");
        // The first three variants are logically the same triangle.
        if !v.contains('<') {
            match &baseline {
                None => baseline = Some(on),
                Some(base) => assert_eq!(&on, base, "all triangle orderings must agree"),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 7. Skip conditions: rule applications and `:limit`-without-`:sort` keep the
//    written order.
// ---------------------------------------------------------------------------

#[test]
fn rule_application_body_is_not_reordered() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite_db(&dir);
    populate(&db, 12, 12);

    // A derived-rule application (`mid`) in the body makes the whole rule
    // ineligible; the plan must equal the written plan.
    let with_rule = "mid[c, p] := *member[c, p]\n\
        ?[c, p1, p2] := mid[c, p1], *member[c, p2], *knows[p1, p2], *knows[p2, p1]";
    let default_sig = plan_sig(&explain(&db, with_rule));
    let written_sig = plan_sig(&explain(&db, &format!("{with_rule} :reorder written")));
    assert_eq!(
        default_sig, written_sig,
        "a body with a rule application must not be reordered"
    );
}

#[test]
fn bare_limit_skips_reorder_but_sorted_limit_reorders() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite_db(&dir);
    populate(&db, 12, 12);

    // `:limit` without `:sort` -> keep the written order (row-subset stability).
    let bare = load_refs(&explain(&db, &format!("{NAIVE} :limit 5")));
    assert_eq!(
        bare,
        vec![":member", ":member", ":member", ":knows", ":knows", ":knows"],
        "a bare :limit must preserve the written order"
    );

    // `:limit` WITH `:sort` -> the output is fully materialized then sorted, so
    // reorder is safe and applies.
    let sorted = load_refs(&explain(&db, &format!("{NAIVE} :order p1 :limit 5")));
    assert_eq!(
        sorted,
        vec![":member", ":member", ":knows", ":member", ":knows", ":knows"],
        "a sorted :limit must still reorder"
    );
}

/// Regression: a multi-valued `in`-unification feeding a non-idempotent
/// aggregation must NOT be reordered — the reorder can otherwise flip the
/// unification between generator (duplicates) and filter, silently changing a
/// count/sum/collect. The body carries such a unification, so the whole rule is
/// ineligible and default (greedy) must equal `:reorder written`. On BOTH
/// backends (the aggregation stream is a bag on either join path).
fn assert_multi_in_aggregation_parity(db: &DbInstance) {
    run_mut(db, ":create r0 { a: Int }");
    run_mut(db, ":create rbig { a: Int, b: Int, c: Int }");
    run_mut(db, ":create ry { a: Int, y: Int }");
    run_mut(db, "?[a] <- [[1]] :put r0 { a }");
    run_mut(db, "?[a,b,c] <- [[1,9,9]] :put rbig { a, b, c }");
    run_mut(db, "?[a,y] <- [[1,5]] :put ry { a, y }");

    // Written order compiles `y in [5,5]` (before *ry binds y) as a generator:
    // two y=5 rows, count = 2. The greedy order would pull *ry ahead, demoting
    // it to a filter (count = 1) — the reorder must decline (ineligible).
    let q = "?[count(y)] := *r0[a], y in [5, 5], *rbig[a, b, c], *ry[a, y]";
    let default = run(db, q).rows[0][0].get_int().unwrap();
    let written = run(db, &format!("{q} :reorder written"))
        .rows[0][0]
        .get_int()
        .unwrap();
    assert_eq!(
        default, written,
        "multi-`in` + aggregation was reordered: default={default} written={written}"
    );
    assert_eq!(default, 2, "written-order semantics: `y in [5,5]` generates two rows");
}

#[test]
fn multi_in_aggregation_not_reordered_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite_db(&dir);
    assert_multi_in_aggregation_parity(&db);
}

#[test]
fn multi_in_aggregation_not_reordered_mem() {
    assert_multi_in_aggregation_parity(&mem_db());
}

// ---------------------------------------------------------------------------
// 8. Regression (LSQB Q3): a PARTIAL composite-key bind must NOT be pulled
//    forward as a keyed expansion. Before the fork's `full_key_lookup_bonus`
//    fix, the greedy tie-break rewarded any atom whose LEADING key column was
//    bound — so on a `knows{src, dst}`-style composite key, binding only `src`
//    scored a "prefix 1" and pulled that fan-out expansion ahead of a selective
//    atom (a benchmarker measured LSQB Q3 go 19s -> timeout). The fix scores a
//    partial prefix 0, so the tie falls to the written order and the demotion
//    never happens. This is the integration analog of the reorder unit test
//    `greedy_ignores_partial_key_prefix_on_tie`, exercised on the real
//    `stored_*` path (sqlite). It FAILS on the pre-fix engine (greedy produced
//    `[:ta, :tc, :tb]`).
// ---------------------------------------------------------------------------

/// `ta{x,y}, tb{z,y}, tc{y,z}` — after the base pick `ta` binds {x,y}, both `tb`
/// and `tc` add exactly one new var (`z`), a tie. `tc`'s key `[y,z]` has its
/// leading column `y` bound (a keyed *expansion* over every `z` for that `y`),
/// while `tb`'s key `[z,y]` has its leading column `z` unbound. The old tie-break
/// wrongly pulled `tc` ahead (`[ta,tc,tb]`); the fix keeps the written order.
fn assert_partial_key_not_pulled_forward(db: &DbInstance, assert_plan: bool) {
    run_mut(db, ":create ta { x: Int, y: Int }");
    run_mut(db, ":create tb { z: Int, y: Int }");
    run_mut(db, ":create tc { y: Int, z: Int }");
    run_mut(db, "?[x, y] <- [[0, 0], [0, 1]] :put ta { x, y }");
    run_mut(db, "?[z, y] <- [[0, 0], [1, 0], [2, 0], [0, 1]] :put tb { z, y }");
    // `tc` is high fan-out on its leading key `y` (8 `z` per `y`) — the shape a
    // partial-key expansion on `y` alone blows up on.
    let mut tc = vec![];
    for y in 0..2i64 {
        for z in 0..8i64 {
            tc.push(DataValue::List(vec![DataValue::from(y), DataValue::from(z)]));
        }
    }
    let mut p = BTreeMap::new();
    p.insert("rows".to_string(), DataValue::List(tc));
    run_mut_p(db, "?[y, z] <- $rows :put tc { y, z }", p);

    let q = "?[x, y, z] := *ta[x, y], *tb[z, y], *tc[y, z]";
    let greedy = sorted_rows(run(db, q));
    let written = sorted_rows(run(db, &format!("{q} :reorder written")));
    assert_eq!(
        greedy, written,
        "partial-key regression: reorder must return the same rows as written order"
    );
    assert_eq!(greedy.len(), 4, "expected 4 triangle rows");

    if assert_plan {
        let greedy_loads = load_refs(&explain(db, q));
        let written_loads = load_refs(&explain(db, &format!("{q} :reorder written")));
        assert_eq!(
            written_loads,
            vec![":ta", ":tb", ":tc"],
            "written order is the authored order"
        );
        assert_eq!(
            greedy_loads, written_loads,
            "a partial composite-key bind (`tc[y, z]`, only `y` bound) must NOT be \
             pulled ahead of the written order — the pre-fix engine demoted it to a \
             keyed expansion and produced [:ta, :tc, :tb]"
        );
    }
}

#[test]
fn partial_key_prefix_not_pulled_forward_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    assert_partial_key_not_pulled_forward(&sqlite_db(&dir), true);
}

#[test]
fn partial_key_prefix_not_pulled_forward_mem() {
    assert_partial_key_not_pulled_forward(&mem_db(), false);
}
