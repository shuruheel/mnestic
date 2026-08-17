/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Per-query memory budget tests (mnestic fork; spec
//! `docs/specs/memory-budget.md`).
//!
//! Pins the budget surface the spec's §11 matrix demands:
//!
//! * The refutation repro, inverted: an intermediate-rule cartesian blowup
//!   under a small budget errors with the distinct `eval::mem_budget_exceeded`
//!   *instead of* materializing (the 2026-07-13 containment attempt errored
//!   only after the store had filled).
//! * The knob triple — in-script `:mem_limit`, per-call
//!   [`ScriptRunOptions::with_mem_limit`], Db default
//!   ([`DbInstance::set_default_query_mem_limit`]) — min-composes: a block or
//!   call can only tighten the budget, never raise a host-set guard.
//! * A tripped mutable script leaves no partial writes; the Db keeps serving;
//!   repeated trip/retry cycles leak nothing.
//! * Every store family (regular, meet, bounded-meet k-set, dominance
//!   skyline) and the sort/result staging copies are charged.
//!
//! Backend note (house rule): budget accounting lives in engine-level temp
//! stores shared by all backends, but the sqlite backend exercises the real
//! `stored_*` join path, so the load-bearing assertions run there; mem gets a
//! parity smoke.

use cozo::{DataValue, DbInstance, NamedRows, ScriptMutability, ScriptRunOptions};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Rows in the base relation; the blowup rule materializes N^2 tuples
/// (~168 B/row measured), so 2,000 rows ⇒ ~4M tuples ≈ ~670 MB unbudgeted.
/// The budgets under test are ≤ 8 MB, so a working budget aborts after a few
/// thousand tuples and the test never pays the quadratic bill.
const N: i64 = 2000;

/// The refutation repro: an *intermediate* rule (not the entry, so the
/// `:limit` early-return machinery never applies) whose store takes the
/// cartesian square of `rel`.
const BLOWUP_QUERY: &str = "tmp[a, b] := *rel[a], *rel[b]\n?[count(a)] := tmp[a, _]";

/// Small enough to finish fast unbudgeted (~40k tuples), big enough that a
/// tight budget still trips on it.
const SMALL_N: i64 = 200;

const ABORT_BOUND: Duration = Duration::from_secs(10);

fn mutable(db: &DbInstance, script: &str) -> NamedRows {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap()
}

fn setup_db(engine: &str, path: &str, n: i64) -> DbInstance {
    let db = DbInstance::new(engine, path, Default::default()).unwrap();
    mutable(&db, ":create rel { a: Int }");
    let rows: Vec<Vec<DataValue>> = (0..n).map(|a| vec![DataValue::from(a)]).collect();
    let mut data = BTreeMap::new();
    data.insert(
        "rel".to_string(),
        NamedRows::new(vec!["a".to_string()], rows),
    );
    db.import_relations(data).unwrap();
    db
}

/// Run a script and, if it errors, extract the miette diagnostic `code` and
/// the full rendered message (via the public JSON folding, so this test crate
/// never names `miette::Report`).
fn run_and_code(
    db: &DbInstance,
    script: &str,
    mutability: ScriptMutability,
    options: ScriptRunOptions,
) -> (bool, String, String, Duration) {
    let t0 = Instant::now();
    let res = db.run_script_with_options(script, BTreeMap::new(), mutability, options);
    let elapsed = t0.elapsed();
    match res {
        Ok(_) => (false, String::new(), String::new(), elapsed),
        Err(err) => {
            let j = cozo::format_error_as_json(err, None);
            let code = j
                .get("code")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            let msg = j
                .get("display")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            (true, code, msg, elapsed)
        }
    }
}

fn assert_budget_trip(label: &str, is_err: bool, code: &str, elapsed: Duration) {
    assert!(
        is_err,
        "[{label}] budgeted blowup query should error, got Ok"
    );
    assert_eq!(
        code, "eval::mem_budget_exceeded",
        "[{label}] budget trip must raise the distinct `eval::mem_budget_exceeded` \
         (got {code:?})"
    );
    assert!(
        elapsed < ABORT_BOUND,
        "[{label}] aborted after {elapsed:?}; the budget is not tripping during \
         materialization — it must error instead of the OOM, not after it"
    );
}

// ---------------------------------------------------------------------------
// 1. The refutation repro, inverted: in-script `:mem_limit`, sqlite + mem.
// ---------------------------------------------------------------------------

fn assert_in_script_limit(engine: &str, path: &str) {
    let db = setup_db(engine, path, N);
    let script = format!("{BLOWUP_QUERY} :mem_limit 4000000");
    let (is_err, code, msg, elapsed) = run_and_code(
        &db,
        &script,
        ScriptMutability::Immutable,
        ScriptRunOptions::default(),
    );
    assert_budget_trip(&format!("{engine}/:mem_limit"), is_err, &code, elapsed);
    assert!(
        msg.contains("bytes") && msg.contains(":mem_limit"),
        "[{engine}] the message names the figures and the knob that tripped: {msg}"
    );
}

#[test]
fn in_script_limit_sqlite() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("mb.db");
    assert_in_script_limit("sqlite", path.to_str().unwrap());
}

#[test]
fn in_script_limit_mem() {
    assert_in_script_limit("mem", "");
}

// ---------------------------------------------------------------------------
// 2. Per-call option trips the same way.
// ---------------------------------------------------------------------------

#[test]
fn per_call_limit_sqlite() {
    let tmp = tempfile::tempdir().unwrap();
    let db = setup_db("sqlite", tmp.path().join("mb.db").to_str().unwrap(), N);
    let (is_err, code, _msg, elapsed) = run_and_code(
        &db,
        BLOWUP_QUERY,
        ScriptMutability::Immutable,
        ScriptRunOptions::default().with_mem_limit(4_000_000),
    );
    assert_budget_trip("sqlite/per-call", is_err, &code, elapsed);
}

// ---------------------------------------------------------------------------
// 3. Db default arms bare queries; blocks/calls can only tighten it.
// ---------------------------------------------------------------------------

#[test]
fn db_default_arms_bare_query() {
    let tmp = tempfile::tempdir().unwrap();
    let db = setup_db("sqlite", tmp.path().join("mb.db").to_str().unwrap(), N);
    db.set_default_query_mem_limit(Some(4_000_000));
    let (is_err, code, _msg, elapsed) = run_and_code(
        &db,
        BLOWUP_QUERY,
        ScriptMutability::Immutable,
        ScriptRunOptions::default(),
    );
    assert_budget_trip("sqlite/db-default", is_err, &code, elapsed);

    // Clearing the default restores unlimited: the small workload passes.
    db.set_default_query_mem_limit(None);
    let small = setup_db(
        "sqlite",
        tmp.path().join("mb_small.db").to_str().unwrap(),
        SMALL_N,
    );
    let (is_err, _, _, _) = run_and_code(
        &small,
        BLOWUP_QUERY,
        ScriptMutability::Immutable,
        ScriptRunOptions::default(),
    );
    assert!(!is_err, "unbudgeted small blowup should complete");
}

#[test]
fn block_limit_cannot_raise_db_default() {
    let tmp = tempfile::tempdir().unwrap();
    let db = setup_db("sqlite", tmp.path().join("mb.db").to_str().unwrap(), N);
    db.set_default_query_mem_limit(Some(4_000_000));
    // The block asks for 100 GB; the host-set default still governs (scripts
    // are increasingly LLM-authored — tighten-only is the signed contract).
    let script = format!("{BLOWUP_QUERY} :mem_limit 100000000000");
    let (is_err, code, _msg, elapsed) = run_and_code(
        &db,
        &script,
        ScriptMutability::Immutable,
        ScriptRunOptions::default(),
    );
    assert_budget_trip("sqlite/tighten-only-block", is_err, &code, elapsed);
}

#[test]
fn per_call_cannot_raise_db_default() {
    let tmp = tempfile::tempdir().unwrap();
    let db = setup_db("sqlite", tmp.path().join("mb.db").to_str().unwrap(), N);
    db.set_default_query_mem_limit(Some(4_000_000));
    let (is_err, code, _msg, elapsed) = run_and_code(
        &db,
        BLOWUP_QUERY,
        ScriptMutability::Immutable,
        ScriptRunOptions::default().with_mem_limit(100_000_000_000),
    );
    assert_budget_trip("sqlite/tighten-only-call", is_err, &code, elapsed);
}

// ---------------------------------------------------------------------------
// 4. Comfortably-under queries succeed with results identical to unbudgeted
//    runs; comfortably-over always errors (the signed determinism posture).
// ---------------------------------------------------------------------------

#[test]
fn under_budget_matches_unbudgeted_and_is_stable() {
    let tmp = tempfile::tempdir().unwrap();
    let db = setup_db(
        "sqlite",
        tmp.path().join("mb.db").to_str().unwrap(),
        SMALL_N,
    );
    let unbudgeted = db
        .run_script(BLOWUP_QUERY, BTreeMap::new(), ScriptMutability::Immutable)
        .unwrap();
    for round in 0..5 {
        let budgeted = db
            .run_script_with_options(
                BLOWUP_QUERY,
                BTreeMap::new(),
                ScriptMutability::Immutable,
                // ~40k tuples × ~168 B ≈ 7 MB total residency; 64 MB is
                // comfortable headroom over stores + staging.
                ScriptRunOptions::default().with_mem_limit(64_000_000),
            )
            .unwrap_or_else(|e| panic!("round {round}: under-budget run errored: {e:?}"));
        assert_eq!(unbudgeted.rows, budgeted.rows, "round {round}");
    }
}

#[test]
fn over_budget_always_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let db = setup_db("sqlite", tmp.path().join("mb.db").to_str().unwrap(), N);
    for round in 0..5 {
        let (is_err, code, _msg, elapsed) = run_and_code(
            &db,
            BLOWUP_QUERY,
            ScriptMutability::Immutable,
            ScriptRunOptions::default().with_mem_limit(4_000_000),
        );
        assert_budget_trip(&format!("sqlite/round-{round}"), is_err, &code, elapsed);
    }
}

// ---------------------------------------------------------------------------
// 5. Trip-path integrity: no partial writes, Db keeps serving, no residue.
// ---------------------------------------------------------------------------

#[test]
fn tripped_mutable_script_leaves_no_partial_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let db = setup_db("sqlite", tmp.path().join("mb.db").to_str().unwrap(), N);
    mutable(&db, ":create out { a: Int, b: Int }");
    let script =
        "tmp[a, b] := *rel[a], *rel[b]\n?[a, b] := tmp[a, b] :put out { a, b } :mem_limit 4000000";
    let (is_err, code, _msg, elapsed) = run_and_code(
        &db,
        script,
        ScriptMutability::Mutable,
        ScriptRunOptions::default(),
    );
    assert_budget_trip("sqlite/mutable", is_err, &code, elapsed);
    let count = db
        .run_script(
            "?[count(a)] := *out[a, _]",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .unwrap();
    assert_eq!(
        count.rows[0][0],
        DataValue::from(0i64),
        "a tripped mutable script must leave no partial writes"
    );
}

#[test]
fn repeated_trips_leave_no_residue() {
    let tmp = tempfile::tempdir().unwrap();
    let db = setup_db("sqlite", tmp.path().join("mb.db").to_str().unwrap(), N);
    for _ in 0..5 {
        let (is_err, code, _msg, _t) = run_and_code(
            &db,
            BLOWUP_QUERY,
            ScriptMutability::Immutable,
            ScriptRunOptions::default().with_mem_limit(4_000_000),
        );
        assert!(is_err && code == "eval::mem_budget_exceeded");
    }
    // The Db is unharmed and an ordinary query still runs.
    let ok = db
        .run_script(
            "?[count(a)] := *rel[a]",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .unwrap();
    assert_eq!(ok.rows[0][0], DataValue::from(N));
}

// ---------------------------------------------------------------------------
// 6. Every store family charges: meet, bounded-meet (k-set), dominance
//    (skyline). Each groups by (a, b), so the store itself must hold ~N^2
//    groups — the budget bites in the aggregated store, not just the regular
//    intermediate.
// ---------------------------------------------------------------------------

fn assert_family_trips(label: &str, script: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let db = setup_db("sqlite", tmp.path().join("mb.db").to_str().unwrap(), N);
    let (is_err, code, _msg, elapsed) = run_and_code(
        &db,
        script,
        ScriptMutability::Immutable,
        ScriptRunOptions::default().with_mem_limit(4_000_000),
    );
    assert_budget_trip(label, is_err, &code, elapsed);
}

#[test]
fn meet_store_trips() {
    assert_family_trips(
        "sqlite/meet",
        "?[a, b, min(x)] := *rel[a], *rel[b], x = a + b :mem_limit 4000000",
    );
}

#[test]
fn bounded_meet_store_trips() {
    assert_family_trips(
        "sqlite/min_cost_k",
        "?[a, b, min_cost_k(p, 3)] := *rel[a], *rel[b], p = [b, 1.0] :mem_limit 4000000",
    );
}

#[test]
fn dominance_store_trips() {
    assert_family_trips(
        "sqlite/pareto",
        "?[a, b, pareto_min(v)] := *rel[a], *rel[b], v = [a, b] :mem_limit 4000000",
    );
}

// ---------------------------------------------------------------------------
// 7. The sort staging copy is charged: a budget the evaluation fits but the
//    sorted duplicate does not still trips.
// ---------------------------------------------------------------------------

#[test]
fn sort_staging_copy_is_charged() {
    let tmp = tempfile::tempdir().unwrap();
    let n = 60_000i64;
    let db = setup_db("sqlite", tmp.path().join("mb.db").to_str().unwrap(), n);
    // One pass over rel (~60k single-int tuples ≈ ~7.7 MB charged in the
    // entry store); `:order` copies all of it again into the sort Vec. A
    // 9 MB budget clears evaluation but not evaluation + the sorted copy.
    let script = "?[a] := *rel[a] :order a :mem_limit 9000000";
    let (is_err, code, _msg, elapsed) = run_and_code(
        &db,
        script,
        ScriptMutability::Immutable,
        ScriptRunOptions::default(),
    );
    assert_budget_trip("sqlite/sort-staging", is_err, &code, elapsed);
    // Control: the same query under a budget with room for both copies.
    let (is_err, _, _, _) = run_and_code(
        &db,
        "?[a] := *rel[a] :order a :mem_limit 64000000",
        ScriptMutability::Immutable,
        ScriptRunOptions::default(),
    );
    assert!(!is_err, "sort within budget should complete");
}

// ---------------------------------------------------------------------------
// 8. Zero clears a block limit (documented `0` semantics), matching
//    `:timeout 0`.
// ---------------------------------------------------------------------------

#[test]
fn zero_block_limit_is_unlimited() {
    let tmp = tempfile::tempdir().unwrap();
    let db = setup_db(
        "sqlite",
        tmp.path().join("mb.db").to_str().unwrap(),
        SMALL_N,
    );
    let script = format!("{BLOWUP_QUERY} :mem_limit 0");
    let (is_err, _, _, _) = run_and_code(
        &db,
        &script,
        ScriptMutability::Immutable,
        ScriptRunOptions::default(),
    );
    assert!(!is_err, ":mem_limit 0 must mean no block limit");
}
