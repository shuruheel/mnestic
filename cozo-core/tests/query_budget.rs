/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Per-query wall-clock budget tests (mnestic fork, 0.10.5).
//!
//! Pins the query-budget surface built on top of the interruptibility fixes:
//!
//! * A budget expiry raises a distinct `eval::timeout` diagnostic (not the
//!   `eval::killed` of an explicit `::kill`).
//! * A budget can be supplied three ways — the in-script `:timeout` option, a
//!   per-call [`DbInstance::run_script_with_options`] timeout, and a Db-wide
//!   default ([`DbInstance::set_default_query_timeout`]) — and the effective
//!   deadline is the *minimum* of whichever are set, so a `:timeout` can only
//!   tighten the budget, never extend past a per-call or Db-default guard.
//! * A per-call budget is a single whole-script deadline: an imperative
//!   multi-statement mutable script aborted by the budget rolls its whole
//!   transaction back, leaving no partial writes.
//!
//! The spin workload is the same "same-group triangle" as
//! `tests/interruptibility.rs`: members-first ordering forces the planner to
//! enumerate a ~N^3 intermediate, so the query runs long enough for the budget
//! to bite well before it would ever complete.

use cozo::{DataValue, DbInstance, NamedRows, ScriptMutability, ScriptRunOptions};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// People in the single group; N^3 candidate triples dominate the abort
/// latency. Matches `tests/interruptibility.rs`.
const N: i64 = 280;

/// The pathological members-first triangle (selective `knows` atoms last).
const SPIN_QUERY: &str = "?[count(x)] := *member[c, a], *member[c, b], *member[c, d], \
     *knows[a, b], *knows[b, d], *knows[d, a], x = [a, b, d]";

/// Generous upper bound on abort latency (the budgets under test are ~1s; the
/// poison-check cadence adds at most a batch of work).
const ABORT_BOUND: Duration = Duration::from_secs(6);

fn mutable(db: &DbInstance, script: &str) -> NamedRows {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap()
}

fn setup_spin_db(engine: &str, path: &str) -> DbInstance {
    let db = DbInstance::new(engine, path, Default::default()).unwrap();
    mutable(&db, ":create member { c: Int, p: Int }");
    mutable(&db, ":create knows { a: Int, b: Int }");

    let members: Vec<Vec<DataValue>> = (0..N)
        .map(|p| vec![DataValue::from(0i64), DataValue::from(p)])
        .collect();
    let mut edges = vec![];
    for a in 0..12i64 {
        for b in 0..12i64 {
            if a != b {
                edges.push(vec![DataValue::from(a), DataValue::from(b)]);
            }
        }
    }
    let mut data = BTreeMap::new();
    data.insert(
        "member".to_string(),
        NamedRows::new(vec!["c".to_string(), "p".to_string()], members),
    );
    data.insert(
        "knows".to_string(),
        NamedRows::new(vec!["a".to_string(), "b".to_string()], edges),
    );
    db.import_relations(data).unwrap();
    db
}

/// Run a script and, if it errors, extract the miette diagnostic `code`. Never
/// names the `miette::Report` type (not a dev-dependency of the test crate):
/// the error is folded to JSON through the public `format_error_as_json`.
fn run_and_code(
    db: &DbInstance,
    script: &str,
    mutability: ScriptMutability,
    options: ScriptRunOptions,
) -> (bool, String, Duration) {
    let t0 = Instant::now();
    let res = db.run_script_with_options(script, BTreeMap::new(), mutability, options);
    let elapsed = t0.elapsed();
    match res {
        Ok(_) => (false, String::new(), elapsed),
        Err(err) => {
            let j = cozo::format_error_as_json(err, None);
            let code = j
                .get("code")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            (true, code, elapsed)
        }
    }
}

fn assert_timeout(label: &str, is_err: bool, code: &str, elapsed: Duration) {
    assert!(is_err, "[{label}] budgeted spin query should error, got Ok");
    assert_eq!(
        code, "eval::timeout",
        "[{label}] budget expiry must raise the distinct `eval::timeout` \
         (got {code:?}); a `::kill` raises `eval::killed`"
    );
    assert!(
        elapsed < ABORT_BOUND,
        "[{label}] aborted after {elapsed:?}; the deadline is not being observed \
         inside RA enumeration"
    );
}

// ---------------------------------------------------------------------------
// 1. In-script `:timeout` raises the distinct `eval::timeout`, on mem + sqlite.
// ---------------------------------------------------------------------------

fn assert_in_script_timeout(engine: &str, path: &str) {
    let db = setup_spin_db(engine, path);
    let script = format!("{SPIN_QUERY} :timeout 1");
    let (is_err, code, elapsed) =
        run_and_code(&db, &script, ScriptMutability::Immutable, ScriptRunOptions::default());
    assert_timeout(&format!("{engine}/:timeout"), is_err, &code, elapsed);
}

#[test]
fn in_script_timeout_mem() {
    assert_in_script_timeout("mem", "");
}

#[test]
fn in_script_timeout_sqlite() {
    // sqlite exercises the real stored_* join path (mem uses a separate
    // operator), per tests/matjoin_regression.rs.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mnestic_budget_inscript.db");
    assert_in_script_timeout("sqlite", path.to_str().unwrap());
}

// ---------------------------------------------------------------------------
// 2. Per-call timeout aborts even with NO `:timeout` in the script text.
// ---------------------------------------------------------------------------

fn assert_per_call_timeout(engine: &str, path: &str) {
    let db = setup_spin_db(engine, path);
    // No `:timeout` in the query text — the budget comes purely from the call.
    let opts = ScriptRunOptions::new().with_timeout(1.0);
    let (is_err, code, elapsed) =
        run_and_code(&db, SPIN_QUERY, ScriptMutability::Immutable, opts);
    assert_timeout(&format!("{engine}/per-call"), is_err, &code, elapsed);
}

#[test]
fn per_call_timeout_mem() {
    assert_per_call_timeout("mem", "");
}

#[test]
fn per_call_timeout_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mnestic_budget_percall.db");
    assert_per_call_timeout("sqlite", path.to_str().unwrap());
}

// ---------------------------------------------------------------------------
// 3. Db default aborts with no per-call / per-block timeout; and a per-block
//    `:timeout 999` cannot extend past a small default (min() precedence).
// ---------------------------------------------------------------------------

#[test]
fn db_default_aborts_bare_query() {
    let db = setup_spin_db("mem", "");
    db.set_default_query_timeout(Some(1.0));
    assert_eq!(db.default_query_timeout(), Some(1.0));

    let (is_err, code, elapsed) =
        run_and_code(&db, SPIN_QUERY, ScriptMutability::Immutable, ScriptRunOptions::default());
    assert_timeout("db-default/bare", is_err, &code, elapsed);
}

#[test]
fn block_timeout_cannot_exceed_db_default() {
    let db = setup_spin_db("mem", "");
    db.set_default_query_timeout(Some(1.0));

    // A generous in-script `:timeout 999` must NOT extend past the 1s default:
    // min() makes the default a guard. If precedence were wrong this would run
    // for ~999s (i.e. far past ABORT_BOUND) and the test would hang/fail.
    let script = format!("{SPIN_QUERY} :timeout 999");
    let (is_err, code, elapsed) =
        run_and_code(&db, &script, ScriptMutability::Immutable, ScriptRunOptions::default());
    assert_timeout("db-default/min", is_err, &code, elapsed);
}

#[test]
fn per_call_cannot_exceed_db_default() {
    let db = setup_spin_db("mem", "");
    db.set_default_query_timeout(Some(1.0));

    // A large per-call timeout is likewise bounded by the small default.
    let opts = ScriptRunOptions::new().with_timeout(999.0);
    let (is_err, code, elapsed) =
        run_and_code(&db, SPIN_QUERY, ScriptMutability::Immutable, opts);
    assert_timeout("db-default/per-call-min", is_err, &code, elapsed);
}

// ---------------------------------------------------------------------------
// 4. A fast query completes normally under a generous budget (no false trip).
// ---------------------------------------------------------------------------

#[test]
fn fast_query_completes_under_budget() {
    let db = DbInstance::new("mem", "", Default::default()).unwrap();

    // Generous per-call budget.
    let opts = ScriptRunOptions::new().with_timeout(30.0);
    let res = db.run_script_with_options(
        "?[x] <- [[1], [2], [3]]",
        BTreeMap::new(),
        ScriptMutability::Immutable,
        opts,
    );
    assert!(res.is_ok(), "fast query tripped a generous per-call budget: {res:?}");
    assert_eq!(res.unwrap().rows.len(), 3);

    // Generous Db default, plus a generous in-script `:timeout`.
    db.set_default_query_timeout(Some(30.0));
    let res = db.run_script(
        "?[x] <- [[1], [2]] :timeout 30",
        BTreeMap::new(),
        ScriptMutability::Immutable,
    );
    assert!(res.is_ok(), "fast query tripped a generous default budget: {res:?}");
    assert_eq!(res.unwrap().rows.len(), 2);

    // Clearing the default restores unbounded behavior.
    db.set_default_query_timeout(None);
    assert_eq!(db.default_query_timeout(), None);
}

// ---------------------------------------------------------------------------
// 5. Imperative multi-statement mutable script + budget: aborts cleanly with
//    no partial commit (whole-script deadline + tx rollback).
// ---------------------------------------------------------------------------

fn assert_imperative_rollback(engine: &str, path: &str) {
    let db = setup_spin_db(engine, path);
    // A pre-existing, committed, empty target relation.
    mutable(&db, ":create target { x: Int }");

    // Imperative program: statement 1 writes a row into `target`; statement 2
    // spins. The whole program shares one transaction and one per-call
    // deadline, so timing out in statement 2 must roll back statement 1's write.
    let script = format!(
        "{{ ?[x] <- [[1]] :put target {{x}} }}\n{{ {SPIN_QUERY} }}"
    );
    let opts = ScriptRunOptions::new().with_timeout(1.0);
    let (is_err, code, elapsed) =
        run_and_code(&db, &script, ScriptMutability::Mutable, opts);
    assert_timeout(&format!("{engine}/imperative"), is_err, &code, elapsed);

    // The write from statement 1 must have been rolled back.
    let after = db
        .run_script("?[x] := *target[x]", BTreeMap::new(), ScriptMutability::Immutable)
        .expect("post-abort read of `target` failed");
    assert!(
        after.rows.is_empty(),
        "[{engine}] budget-aborted imperative script left a partial write in \
         `target`: {:?}",
        after.rows
    );

    // The db is still usable after the abort.
    mutable(&db, "?[x] <- [[7]] :put target {x}");
    let after = db
        .run_script("?[x] := *target[x]", BTreeMap::new(), ScriptMutability::Immutable)
        .unwrap();
    assert_eq!(after.rows.len(), 1);
}

#[test]
fn imperative_budget_rollback_mem() {
    assert_imperative_rollback("mem", "");
}

#[test]
fn imperative_budget_rollback_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mnestic_budget_imperative.db");
    assert_imperative_rollback("sqlite", path.to_str().unwrap());
}

// ---------------------------------------------------------------------------
// 7. Absurd / non-finite budgets must not panic (regression: the deadline was
//    built with `Duration::from_secs_f64` + `Instant + Duration`, which panic
//    on infinite/overflowing input — remotely reachable via `:timeout` text
//    and the cozo-bin HTTP `timeout` field). They now clamp to "no deadline".
// ---------------------------------------------------------------------------

#[test]
fn huge_and_infinite_budgets_do_not_panic() {
    let db = DbInstance::new("mem", "", Default::default()).unwrap();

    // In-script `:timeout` with an absurd value: parses to a finite-but-huge or
    // infinite f64; must not panic, and a trivial query still completes.
    for script in ["?[x] <- [[1]] :timeout 1e300", "?[x] <- [[1]] :timeout 1e309"] {
        let (is_err, code, _) =
            run_and_code(&db, script, ScriptMutability::Immutable, ScriptRunOptions::default());
        assert!(!is_err, "{script} should complete, got error {code:?}");
    }

    // Per-call timeout of +inf / f64::MAX: must not panic; query completes.
    for secs in [f64::INFINITY, f64::MAX] {
        let (is_err, code, _) = run_and_code(
            &db,
            "?[x] <- [[1]]",
            ScriptMutability::Immutable,
            ScriptRunOptions::new().with_timeout(secs),
        );
        assert!(!is_err, "per-call timeout {secs} should complete, got {code:?}");
    }

    // Db-wide default of f64::MAX: saturates, must not panic; query completes.
    db.set_default_query_timeout(Some(f64::MAX));
    let (is_err, code, _) = run_and_code(
        &db,
        "?[x] <- [[1]]",
        ScriptMutability::Immutable,
        ScriptRunOptions::default(),
    );
    assert!(!is_err, "huge Db default should complete, got {code:?}");
}
