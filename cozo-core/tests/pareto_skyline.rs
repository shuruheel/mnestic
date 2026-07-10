/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Built-in skyline (Pareto-frontier) aggregates `pareto_min` / `pareto_max`
//! (mnestic fork). Unlike the host-registered `antichain`
//! (`antichain_bounded_meet.rs`), these need NO registration — they are
//! reserved builtins reachable from plain CozoScript, hence from every binding
//! with zero per-binding work. That is the delivery-surface answer to
//! `docs/specs/antichain-bounded-meet.md` §5 for bound/served consumers.
//!
//! They ride the same `DominanceMeetStore` as the registered dominance, so the
//! dedup / dominance / recursion machinery — and its confluence and cycle
//! pruning — is already covered by `antichain_bounded_meet.rs`. This suite pins
//! what is new: the native `pareto_dominates` comparator (min and max, N-D, the
//! sign-flip idiom for mixed objectives), the operand contract that a `bool`
//! dominance cannot report, and reachability without registration. Sqlite
//! backend per the repo test-backend rule.

use cozo::{DataValue, DbInstance, ScriptMutability};
use std::collections::BTreeMap;

fn open_db() -> (tempfile::TempDir, DbInstance) {
    let dir = tempfile::tempdir().unwrap();
    let db = DbInstance::new("sqlite", dir.path().join("sky.db").to_str().unwrap(), "").unwrap();
    (dir, db)
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

/// The output survivors come back in canonical memcmp order (the store is a
/// `BTreeMap` over survivor tuples), so the expected sets below are written in
/// that order and compared exactly — a degenerate implementation that failed to
/// prune (e.g. `collect`) would keep the dominated points and fail.
#[test]
fn pareto_min_and_max_frontiers_no_registration() {
    let (_dir, db) = open_db();
    let run = |s: &str| {
        db.run_script(s, BTreeMap::new(), ScriptMutability::Mutable)
            .unwrap_or_else(|e| panic!("script failed: {s}\n{e:?}"))
    };

    // Five 2-D points. Minimizing both: [3,3] is dominated by [2,2], and
    // [2,6] by both [1,5] and [2,2]; the rest are mutually incomparable.
    let data = r#"[["g", [1, 5]], ["g", [2, 2]], ["g", [5, 1]], ["g", [3, 3]], ["g", [2, 6]]]"#;

    let res = run(&format!(
        "cand[g, p] <- {data}\nsurv[g, pareto_min(p)] := cand[g, p]\n?[g, p] := surv[g, p]"
    ));
    assert_eq!(
        packs_of(&res.rows),
        vec![pack(&[1, 5]), pack(&[2, 2]), pack(&[5, 1])],
        "pareto_min keeps the lower-left frontier, drops [3,3] and [2,6]"
    );

    // Maximizing both flips dominance: [2,2] and [3,3]-vs? [3,3] survives (no
    // point is >= it on both), [1,5] is dominated by [2,6], [2,2] by [3,3].
    let res = run(&format!(
        "cand[g, p] <- {data}\nsurv[g, pareto_max(p)] := cand[g, p]\n?[g, p] := surv[g, p]"
    ));
    assert_eq!(
        packs_of(&res.rows),
        vec![pack(&[2, 6]), pack(&[3, 3]), pack(&[5, 1])],
        "pareto_max keeps the upper-right frontier"
    );
}

#[test]
fn one_dimensional_reduces_to_min_and_max() {
    let (_dir, db) = open_db();
    let run = |s: &str| {
        db.run_script(s, BTreeMap::new(), ScriptMutability::Mutable)
            .unwrap()
    };
    let data = r#"[["g", [3]], ["g", [1]], ["g", [2]], ["g", [1]]]"#;
    let res = run(&format!(
        "cand[g, p] <- {data}\nsurv[g, pareto_min(p)] := cand[g, p]\n?[g, p] := surv[g, p]"
    ));
    assert_eq!(packs_of(&res.rows), vec![pack(&[1])], "1-D min = the minimum, dedup'd");
    let res = run(&format!(
        "cand[g, p] <- {data}\nsurv[g, pareto_max(p)] := cand[g, p]\n?[g, p] := surv[g, p]"
    ));
    assert_eq!(packs_of(&res.rows), vec![pack(&[3])], "1-D max = the maximum");
}

#[test]
fn groups_are_independent() {
    let (_dir, db) = open_db();
    let res = db
        .run_script(
            r#"
        cand[g, p] <- [["a", [1, 2]], ["a", [2, 1]], ["a", [2, 2]],
                       ["b", [5, 5]], ["b", [9, 1]]]
        surv[g, pareto_min(p)] := cand[g, p]
        ?[g, p] := surv[g, p]
        "#,
            BTreeMap::new(),
            ScriptMutability::Mutable,
        )
        .unwrap();
    // group a: [2,2] dominated by [1,2] and [2,1]; group b: both incomparable.
    let rows: Vec<(String, DataValue)> = res
        .rows
        .iter()
        .map(|r| (r[0].get_str().unwrap().to_string(), r[1].clone()))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("a".to_string(), pack(&[1, 2])),
            ("a".to_string(), pack(&[2, 1])),
            ("b".to_string(), pack(&[5, 5])),
            ("b".to_string(), pack(&[9, 1])),
        ],
        "each group computes its own frontier"
    );
}

#[test]
fn three_dimensional_frontier_prunes_the_dominated() {
    let (_dir, db) = open_db();
    let res = db
        .run_script(
            r#"
        cand[g, p] <- [["g", [1, 2, 3]], ["g", [2, 1, 3]], ["g", [3, 3, 1]],
                       ["g", [2, 2, 2]], ["g", [3, 3, 3]]]
        surv[g, pareto_min(p)] := cand[g, p]
        ?[g, p] := surv[g, p]
        "#,
            BTreeMap::new(),
            ScriptMutability::Mutable,
        )
        .unwrap();
    // [3,3,3] is dominated by [2,2,2]; the other four are mutually incomparable
    // (3-D skylines are naturally large — the whole point of not truncating).
    assert_eq!(
        packs_of(&res.rows),
        vec![
            pack(&[1, 2, 3]),
            pack(&[2, 1, 3]),
            pack(&[2, 2, 2]),
            pack(&[3, 3, 1]),
        ],
        "[3,3,3] pruned, the incomparable frontier kept"
    );
}

#[test]
fn mixed_objective_via_sign_flip() {
    // The documented idiom: minimize price AND maximize quality by negating the
    // maximised component and using pareto_min. Items (price, quality):
    // A(10,5) B(20,8) C(30,9) trace a genuine cost/quality tradeoff (all on the
    // frontier); D(25,7) is beaten by B (cheaper and better) and is pruned.
    let (_dir, db) = open_db();
    let res = db
        .run_script(
            r#"
        item[name, price, quality] <- [["A", 10, 5], ["B", 20, 8], ["C", 30, 9], ["D", 25, 7]]
        surv[pareto_min(p)] := item[_, price, quality], p = [price, -quality]
        ?[p] := surv[p]
        "#,
            BTreeMap::new(),
            ScriptMutability::Mutable,
        )
        .unwrap();
    let packs: Vec<DataValue> = res.rows.iter().map(|r| r[0].clone()).collect();
    assert_eq!(
        packs,
        vec![pack(&[10, -5]), pack(&[20, -8]), pack(&[30, -9])],
        "the contested cost/quality frontier survives; the dominated D is gone"
    );
}

#[test]
fn recursive_multi_objective_reachability() {
    // Rides the DominanceMeetStore's recursion admission exactly like the
    // registered `antichain` (antichain_bounded_meet.rs case (b)): the frontier
    // of path-cost vectors from "start", with a cycle that must be pruned.
    let (_dir, db) = open_db();
    let run = |s: &str| {
        db.run_script(s, BTreeMap::new(), ScriptMutability::Mutable)
            .unwrap_or_else(|e| panic!("script failed: {s}\n{e:?}"))
    };
    run(":create edge2 {src: String, dst: String => dx: Int, dy: Int}");
    run(
        r#"?[src, dst, dx, dy] <- [
            ["start", "a", 1, 3], ["start", "b", 3, 1], ["a", "end", 1, 3],
            ["b", "end", 3, 1], ["start", "end", 3, 3], ["end", "a", 1, 1]
        ] :put edge2 {src, dst => dx, dy}"#,
    );
    let res = run(
        r#"
        reach[t, pareto_min(p)] := *edge2{src: "start", dst: t, dx, dy}, p = [dx, dy]
        reach[t, pareto_min(p)] := reach[m, q], *edge2{src: m, dst: t, dx, dy},
                                   p = [get(q, 0) + dx, get(q, 1) + dy]
        ?[t, p] := reach[t, p], t = "end"
        "#,
    );
    assert_eq!(
        packs_of(&res.rows),
        vec![pack(&[2, 6]), pack(&[3, 3]), pack(&[6, 2])],
        "Pareto frontier of path costs at 'end', cycle pruned"
    );
}

#[test]
fn operand_contract_is_enforced_loudly() {
    let (_dir, db) = open_db();
    let run = |s: &str| db.run_script(s, BTreeMap::new(), ScriptMutability::Mutable);

    // Non-list operand.
    let err = run("cand[g, x] <- [[\"g\", 3]]\nsurv[g, pareto_min(x)] := cand[g, x]\n?[g, p] := surv[g, p]")
        .unwrap_err();
    assert!(
        errstr(&err).contains("list of numbers"),
        "non-list rejected: {err:?}"
    );

    // Non-numeric component.
    let err = run("cand[g, p] <- [[\"g\", [\"x\", 1]]]\nsurv[g, pareto_min(p)] := cand[g, p]\n?[g, q] := surv[g, q]")
        .unwrap_err();
    assert!(
        errstr(&err).contains("must be a number"),
        "non-numeric component rejected: {err:?}"
    );

    // Empty vector.
    let err = run("cand[g, p] <- [[\"g\", []]]\nsurv[g, pareto_min(p)] := cand[g, p]\n?[g, q] := surv[g, q]")
        .unwrap_err();
    assert!(
        errstr(&err).contains("non-empty"),
        "empty vector rejected: {err:?}"
    );
}

#[test]
fn call_site_arguments_are_rejected() {
    let (_dir, db) = open_db();
    let err = db
        .run_script(
            "cand[g, p] <- [[\"g\", [1, 1]]]\nsurv[g, pareto_min(p, 3)] := cand[g, p]\n?[g, q] := surv[g, q]",
            BTreeMap::new(),
            ScriptMutability::Mutable,
        )
        .unwrap_err();
    assert!(
        errstr(&err).contains("takes no arguments"),
        "call-site args rejected: {err:?}"
    );
}

#[test]
fn builtin_names_are_reserved_against_registration() {
    let (_dir, db) = open_db();
    let err = db
        .register_bounded_meet_aggr("pareto_min".to_string(), |_a, _b| false, 8)
        .unwrap_err();
    assert!(
        errstr(&err).contains("reserved"),
        "pareto_min is a reserved builtin name: {err:?}"
    );
}
