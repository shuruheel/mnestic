/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Test matrix for `docs/specs/antichain-bounded-meet.md` §6 — the dominance
//! bounded-meet (antichain / skyline) aggregate. Sqlite backend per the repo
//! test-backend rule.

use cozo::{DataValue, DbInstance, ScriptMutability};
use std::collections::BTreeMap;

/// 2-D Pareto dominance over `[x, y]` packs, minimizing both coordinates:
/// a dominates b iff a is ≤ in both and < in at least one.
fn pareto2(a: &DataValue, b: &DataValue) -> bool {
    let g = |v: &DataValue, i: usize| -> f64 {
        match v {
            DataValue::List(l) => l
                .get(i)
                .and_then(|x| x.get_float())
                .unwrap_or(f64::INFINITY),
            _ => f64::INFINITY,
        }
    };
    let (ax, ay, bx, by) = (g(a, 0), g(a, 1), g(b, 0), g(b, 1));
    ax <= bx && ay <= by && (ax < bx || ay < by)
}

fn open_db(path: &std::path::Path) -> DbInstance {
    DbInstance::new("sqlite", path.to_str().unwrap(), Default::default()).unwrap()
}

fn packs_of(rows: &[Vec<DataValue>]) -> Vec<DataValue> {
    rows.iter().map(|r| r[1].clone()).collect()
}

fn pack(xs: &[i64]) -> DataValue {
    DataValue::List(xs.iter().map(|x| DataValue::from(*x)).collect())
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

#[test]
fn antichain_test_matrix() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("antichain.db");
    let db = open_db(&path);
    db.register_bounded_meet_aggr("antichain".to_string(), pareto2, 64)
        .unwrap();

    let run = |script: &str| db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable);
    let run_ok =
        |script: &str| run(script).unwrap_or_else(|e| panic!("script failed: {script}\n{e:?}"));

    // ---- (a) + (k): non-recursive 2-D Pareto frontier, known answer ----
    // [3,3] dominated by [2,2]; [2,6] dominated by [1,5]; the rest incomparable.
    let res = run_ok(
        r#"
        candidate[g, p] <- [["cause", [1, 5]], ["cause", [2, 2]], ["cause", [5, 1]],
                            ["cause", [3, 3]], ["cause", [2, 6]]]
        surv[g, antichain(p)] := candidate[g, p]
        ?[g, p] := surv[g, p]
        "#,
    );
    assert_eq!(
        packs_of(&res.rows),
        vec![pack(&[1, 5]), pack(&[2, 2]), pack(&[5, 1])],
        "(a) frontier in canonical memcmp order"
    );

    // ---- (c): equal payloads dedup; containment chain in worst arrival
    // order; one insert evicting several survivors (multi-removal) ----
    let res = run_ok(
        r#"
        candidate[g, p] <- [["g", [3, 3]], ["g", [3, 3]], ["g", [2, 2]], ["g", [2, 2]]]
        surv[g, antichain(p)] := candidate[g, p]
        ?[g, p] := surv[g, p]
        "#,
    );
    assert_eq!(
        packs_of(&res.rows),
        vec![pack(&[2, 2])],
        "(c) dedup + chain"
    );
    let res = run_ok(
        r#"
        candidate[g, p] <- [["g", [2, 5]], ["g", [5, 2]], ["g", [1, 1]]]
        surv[g, antichain(p)] := candidate[g, p]
        ?[g, p] := surv[g, p]
        "#,
    );
    assert_eq!(
        packs_of(&res.rows),
        vec![pack(&[1, 1])],
        "(c) one insert evicts two incomparable survivors"
    );

    // ---- (b) + (f-permitted): recursion — multi-objective reachability
    // with a cycle; converges to the Pareto frontier of path-cost vectors ----
    run_ok(":create edge2 {src: String, dst: String => dx: Int, dy: Int}");
    run_ok(
        r#"?[src, dst, dx, dy] <- [
            ["start", "a", 1, 3], ["start", "b", 3, 1], ["a", "end", 1, 3],
            ["b", "end", 3, 1], ["start", "end", 3, 3], ["end", "a", 1, 1]
        ] :put edge2 {src, dst => dx, dy}"#,
    );
    let res = run_ok(
        r#"
        reach[t, antichain(p)] := *edge2{src: "start", dst: t, dx, dy}, p = [dx, dy]
        reach[t, antichain(p)] := reach[m, q], *edge2{src: m, dst: t, dx, dy},
                                  p = [get(q, 0) + dx, get(q, 1) + dy]
        ?[t, p] := reach[t, p], t = "end"
        "#,
    );
    assert_eq!(
        packs_of(&res.rows),
        vec![pack(&[2, 6]), pack(&[3, 3]), pack(&[6, 2])],
        "(b) Pareto frontier of path costs at 'end', cycle pruned"
    );

    // ---- (i): confluence — permuted arrival orders, byte-identical output ----
    let orders = [
        r#"[["g", [1, 5]], ["g", [2, 2]], ["g", [5, 1]], ["g", [3, 3]], ["g", [2, 6]]]"#,
        r#"[["g", [2, 6]], ["g", [3, 3]], ["g", [5, 1]], ["g", [2, 2]], ["g", [1, 5]]]"#,
        r#"[["g", [3, 3]], ["g", [1, 5]], ["g", [2, 6]], ["g", [5, 1]], ["g", [2, 2]]]"#,
    ];
    let mut outs = vec![];
    for o in orders {
        let res = run_ok(&format!(
            "candidate[g, p] <- {o}\nsurv[g, antichain(p)] := candidate[g, p]\n?[g, p] := surv[g, p]"
        ));
        outs.push(res.rows);
    }
    assert_eq!(outs[0], outs[1], "(i) confluent under permutation");
    assert_eq!(outs[0], outs[2], "(i) confluent under permutation");
    assert_eq!(
        packs_of(&outs[0]),
        vec![pack(&[1, 5]), pack(&[2, 2]), pack(&[5, 1])],
        "(i) and equal to the canonical frontier, not merely mutually equal"
    );

    // ---- (d): max_survivors overflow is a loud error, never truncation ----
    db.register_bounded_meet_aggr("antichain_tiny".to_string(), pareto2, 2)
        .unwrap();
    let err = run(r#"
        candidate[g, p] <- [["g", [1, 9]], ["g", [5, 5]], ["g", [9, 1]]]
        surv[g, antichain_tiny(p)] := candidate[g, p]
        ?[g, p] := surv[g, p]
        "#)
    .unwrap_err();
    assert!(
        errstr(&err).contains("max_survivors"),
        "(d) overflow names the guard: {err:?}"
    );

    // ---- call-site args rejected loudly ----
    let err = run(r#"
        candidate[g, p] <- [["g", [1, 1]]]
        surv[g, antichain(p, 3)] := candidate[g, p]
        ?[g, p] := surv[g, p]
        "#)
    .unwrap_err();
    assert!(
        errstr(&err).contains("takes no arguments"),
        "call-site args rejected: {err:?}"
    );

    // ---- (e): debug probes fire on lawless closures ----
    #[cfg(debug_assertions)]
    {
        db.register_bounded_meet_aggr("bad_reflexive".to_string(), |_a, _b| true, 8)
            .unwrap();
        let err = run(r#"
            candidate[g, p] <- [["g", [1, 1]]]
            surv[g, bad_reflexive(p)] := candidate[g, p]
            ?[g, p] := surv[g, p]
            "#)
        .unwrap_err();
        assert!(
            errstr(&err).contains("irreflexivity"),
            "(e) reflexive closure caught: {err:?}"
        );

        db.register_bounded_meet_aggr("bad_symmetric".to_string(), |a, b| a != b, 8)
            .unwrap();
        let err = run(r#"
            candidate[g, p] <- [["g", [1, 1]], ["g", [2, 2]]]
            surv[g, bad_symmetric(p)] := candidate[g, p]
            ?[g, p] := surv[g, p]
            "#)
        .unwrap_err();
        assert!(
            errstr(&err).contains("asymmetry"),
            "(e) symmetric closure caught: {err:?}"
        );
    }

    // ---- (f): non-meet aggregates still rejected in recursion ----
    let err = run("r[collect(x)] := r[prev], x = 1\n?[z] := r[z]").unwrap_err();
    assert!(
        !errstr(&err).is_empty(),
        "(f) recursive normal aggregate still rejected"
    );

    // ---- (g): trigger scripts referencing a registered bounded-meet are
    // rejected at ::set_triggers (validated against empty registries) ----
    run_ok(":create trig_target {a: String}");
    let err = run(r#"::set_triggers trig_target
        on put {
            x[g, antichain(p)] := _new[g], p = [1, 1]
            ?[g, p] := x[g, p]
        }
        "#)
    .unwrap_err();
    assert!(
        errstr(&err).contains("antichain"),
        "(g) trigger referencing registered bounded-meet rejected: {err:?}"
    );

    // ---- (h): registration policy ----
    let e = db
        .register_bounded_meet_aggr("min_cost_k".to_string(), pareto2, 8)
        .unwrap_err();
    assert!(errstr(&e).contains("builtin names"), "(h) {e:?}");
    let e = db
        .register_bounded_meet_aggr("coalesce".to_string(), pareto2, 8)
        .unwrap_err();
    assert!(errstr(&e).contains("builtin function names"), "(h) {e:?}");
    let e = db
        .register_bounded_meet_aggr("antichain".to_string(), pareto2, 8)
        .unwrap_err();
    assert!(errstr(&e).contains("already registered"), "(h) {e:?}");
    let e = db
        .register_bounded_meet_aggr("zero_cap".to_string(), pareto2, 0)
        .unwrap_err();
    assert!(errstr(&e).contains("at least 1"), "(h) {e:?}");
    let e = db
        .register_bounded_meet_aggr("Bad_Name".to_string(), pareto2, 8)
        .unwrap_err();
    assert!(errstr(&e).contains("lowercase"), "(h) {e:?}");
    // cross-registry collisions, both directions
    db.register_custom_aggr("crossed".to_string(), true, || {
        unreachable!("factory never called in this test")
    })
    .unwrap();
    let e = db
        .register_bounded_meet_aggr("crossed".to_string(), pareto2, 8)
        .unwrap_err();
    assert!(errstr(&e).contains("already registered"), "(h) {e:?}");
    let e = db
        .register_custom_aggr("antichain".to_string(), true, || {
            unreachable!("factory never called in this test")
        })
        .unwrap_err();
    assert!(errstr(&e).contains("already registered"), "(h) {e:?}");
    // retrofit: register_custom_aggr also rejects builtin FUNCTION names now
    let e = db
        .register_custom_aggr("length".to_string(), true, || {
            unreachable!("factory never called in this test")
        })
        .unwrap_err();
    assert!(errstr(&e).contains("builtin function names"), "(h) {e:?}");

    // ---- (j): persistence — output rows are plain data; reopen without
    // registration reads them; recompute without registration errors ----
    run_ok(":create frontier {g: String, p: [Int] => }");
    run_ok(
        r#"
        candidate[g, p] <- [["cause", [1, 5]], ["cause", [2, 2]], ["cause", [3, 3]]]
        surv[g, antichain(p)] := candidate[g, p]
        ?[g, p] := surv[g, p]
        :put frontier {g, p}
        "#,
    );
    drop(db);
    let db2 = open_db(&path);
    let res = db2
        .run_script(
            "?[g, p] := *frontier[g, p]",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .unwrap();
    assert_eq!(
        packs_of(&res.rows),
        vec![pack(&[1, 5]), pack(&[2, 2])],
        "(j) persisted antichain rows readable with no registration"
    );
    let err = db2
        .run_script(
            r#"
            candidate[g, p] <- [["cause", [1, 5]]]
            surv[g, antichain(p)] := candidate[g, p]
            ?[g, p] := surv[g, p]
            "#,
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .unwrap_err();
    assert!(
        errstr(&err).contains("antichain"),
        "(j) recompute without registration errors loudly: {err:?}"
    );
}
