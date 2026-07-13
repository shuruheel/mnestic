/*
 * Copyright 2023, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The validity float channel — mnestic 0.12.2. Design: `docs/plans/mnestic-0121-0130/design-0122.md`.
//!
//! ONE bug, FOUR sites, all funnelling through `Num::get_int`, which accepts any *integral*
//! float. Validity/tx-time stamps are microseconds; `now()` and `parse_timestamp()` return
//! float *seconds*. So a float in a validity position was silently denominated 1e6 too small
//! and landed in 1970 — before any row was asserted.
//!
//! Each test below is named for the site it guards, and each goes RED when that site alone is
//! reverted (`get_int_strict` -> `get_int`). A test that survives its own revert guards nothing.
//!
//!   site 1  parse/query.rs  expr2vld_spec   read, valid-time `@`
//!   site 2  parse/query.rs  expr2tt_spec    read, tx-time `@ (tt: …)` AND `:as_of`
//!   site 3  data/functions.rs  op_validity  the `validity(...)` constructor
//!   site 4  data/relation.rs  ColType::Validity List arm   THE WRITE PATH

use cozo::{DbInstance, ScriptMutability};
use serde_json::json;

fn db() -> DbInstance {
    DbInstance::new("mem", "", "").unwrap()
}

fn run(db: &DbInstance, script: &str) -> Result<serde_json::Value, String> {
    db.run_script(script, Default::default(), ScriptMutability::Mutable)
        .map(|r| json!(r.rows))
        .map_err(|e| format!("{e:?}"))
}

/// A relation with one row asserted at 2024-06-01T00:00:00Z == 1_717_200_000_000_000 µs.
const MICROS_2024_06_01: i64 = 1_717_200_000_000_000;

fn seeded() -> DbInstance {
    let db = db();
    run(&db, ":create ev {k: String, at: Validity => v: String}").unwrap();
    run(
        &db,
        &format!("?[k, at, v] <- [['a', [{MICROS_2024_06_01}, true], 'JUN2024']] :put ev {{k, at => v}}"),
    )
    .unwrap();
    db
}

// ─────────────────────────── site 4: THE WRITE PATH (the important one) ───────────────────────────

/// The corruption that nobody had named. Before the fix this returned `Ok` and stored the row
/// at valid-time 1_717_200_000 µs — 1970-01-01T00:28:37Z, not 2024 — which reads back fine on an
/// ordinary query and is visible only under time travel.
///
/// Deliberately a *bare list literal into a Validity column*: it must not route through
/// `validity(...)` (site 3 would catch it) and must not sit in an `@` clause (site 1 would).
/// That isolates site 4.
#[test]
fn float_validity_write_is_rejected() {
    let db = db();
    run(&db, ":create ev {k: String, at: Validity => v: String}").unwrap();

    let res = run(
        &db,
        "?[k, at, v] <- [['a', [parse_timestamp('2024-06-01T00:00:00Z'), true], 'X']] \
         :put ev {k, at => v}",
    );
    let err = res.expect_err("a float valid-time must be REJECTED, not silently stored at 1970");
    assert!(
        err.contains("float"),
        "the error must name the float, got: {err}"
    );
    assert!(
        err.contains("to_int"),
        "the error must tell the caller what to write instead, got: {err}"
    );

    // The corruption oracle: nothing may be on disk at a 1970-scale timestamp.
    let rows = run(&db, "?[k, at, v] := *ev{k, at, v @ 'END'}").unwrap();
    assert_eq!(
        rows.as_array().unwrap().len(),
        0,
        "the rejected write must not have landed: {rows}"
    );
}

/// The same corruption via the other write spelling — `validity(...)` into a Validity column.
///
/// This guards **site 3, not site 4** — verified by reverting each site in isolation. A
/// `validity(...)` call produces a `DataValue::Validity`, which enters the column through the
/// passthrough arm (`vld @ DataValue::Validity(_) => vld`) and never reaches the `List` arm
/// that site 4 guards. So fixing only the write path would have left this spelling corrupting.
/// That is the whole reason the constructor is a site in its own right.
#[test]
fn float_validity_ctor_write_is_rejected() {
    let db = db();
    run(&db, ":create ev {k: String, at: Validity => v: String}").unwrap();
    let res = run(
        &db,
        "?[k, at, v] <- [['a', validity(parse_timestamp('2024-06-01T00:00:00Z')), 'X']] \
         :put ev {k, at => v}",
    );
    assert!(
        res.is_err(),
        "validity(<float>) must not silently stamp 1970: {res:?}"
    );
}

// ─────────────────────────── site 1: read, valid-time ───────────────────────────

#[test]
fn float_vt_selector_is_rejected() {
    let db = seeded();
    let err = run(
        &db,
        "?[k, v] := *ev{k, v @ parse_timestamp('2024-06-01T00:00:00Z')}",
    )
    .expect_err("a float valid-time selector must be REJECTED, not silently read as 1970");
    assert!(err.contains("valid-time"), "must name the axis: {err}");
    assert!(err.contains("to_int"), "must name the fix: {err}");
}

/// `@ 1e300` used to be accepted and saturate to `i64::MAX` — i.e. silently query the end of
/// time. Rejecting Float closes this for free; no magnitude gate needed (design-0122 §3.5).
#[test]
fn float_saturation_is_rejected() {
    let db = seeded();
    assert!(
        run(&db, "?[k, v] := *ev{k, v @ 1e300}").is_err(),
        "@ 1e300 must not silently saturate to end-of-time"
    );
}

/// `round()` returns a *float* (`op_round`, data/functions.rs), so it is the repair an agent
/// reaches for first and it does NOT work. It must fail LOUDLY, not silently read 1970 —
/// otherwise the LLM repair loop never terminates.
#[test]
fn float_round_is_rejected_loudly() {
    let db = seeded();
    assert!(
        run(&db, "?[k, v] := *ev{k, v @ round(now())}").is_err(),
        "@ round(now()) must be a loud error, not a silent 1970 read"
    );
}

// ─────────────────────────── site 2: read, transaction-time (+ :as_of) ───────────────────────────

fn seeded_tt() -> DbInstance {
    let db = db();
    // A TxTime column is load-bearing: `:as_of` against a relation with no transaction-time
    // stamp errors BEFORE the timestamp is ever parsed, which would make these tests
    // non-discriminating. Modelled on `as_of_pins_the_whole_query` in runtime/tests.rs.
    run(&db, ":create audit {k: String, tt: TxTime => v: Int}").unwrap();
    run(&db, "?[k, v] <- [['a', 1]] :put audit {k => v}").unwrap();
    db
}

#[test]
fn float_tt_selector_is_rejected() {
    let db = seeded_tt();
    let err = run(
        &db,
        "?[k, v] := *audit{k, v @ (tt: parse_timestamp('2024-06-01T00:00:00Z'))}",
    )
    .expect_err("a float tx-time selector must be rejected");
    assert!(
        err.contains("transaction-time"),
        "must name the axis: {err}"
    );
}

#[test]
fn float_as_of_is_rejected() {
    let db = seeded_tt();
    assert!(
        run(
            &db,
            "?[k, v] := *audit{k, v} :as_of parse_timestamp('2024-06-01T00:00:00Z')"
        )
        .is_err(),
        ":as_of with a float must be rejected (it routes through expr2tt_spec)"
    );
}

// ─────────────────────────── site 3: the constructor, in isolation ───────────────────────────

/// Deliberately NOT in `@` position and NOT written into a Validity column, so neither site 1
/// nor site 4 can mask it. This is the only test that isolates `op_validity`.
#[test]
fn op_validity_rejects_float() {
    let db = db();
    let err = run(
        &db,
        "?[v] := v = validity(parse_timestamp('2024-06-01T00:00:00Z'))",
    )
    .expect_err("validity(<float>) must be rejected");
    assert!(
        err.contains("MICROSECONDS") || err.contains("float"),
        "must explain the unit: {err}"
    );
}

// ═══════════════════════════ CONTROLS — the regression fence ═══════════════════════════
// Every one of these passes on HEAD and must still pass. They are what stops someone
// "improving" this fix into a magnitude gate (design-0122 §5).

#[test]
fn control_int_micros_still_works() {
    let db = seeded();
    let rows = run(&db, &format!("?[k, v] := *ev{{k, v @ {MICROS_2024_06_01}}}")).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 1, "int micros must work");
}

#[test]
fn control_string_forms_still_work() {
    let db = seeded();
    for sel in [
        "'2024-06-01'",              // bare date (fork feature, 0.10.0)
        "'2024-06-01T00:00:00Z'",    // RFC3339 (upstream)
        "'END'",
    ] {
        let rows = run(&db, &format!("?[k, v] := *ev{{k, v @ {sel}}}")).unwrap();
        assert_eq!(
            rows.as_array().unwrap().len(),
            1,
            "@ {sel} must still return the row"
        );
    }
    // 'NOW' resolves to the wall clock, which is after the 2024 assertion.
    let rows = run(&db, "?[k, v] := *ev{k, v @ 'NOW'}").unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 1, "@ 'NOW' must still work");
}

/// THE FENCE AGAINST A MAGNITUDE GATE. Valid time is deliberately an abstract, user-set
/// logical clock — the engine's own tests use `@ 250`, and the public tutorial uses `@ 2019`.
/// Any "helpful" check that rejects implausibly-small timestamps breaks all of it.
#[test]
fn control_small_abstract_ints_still_work() {
    let db = db();
    run(&db, ":create hos {state: String, year: Validity => v: Int}").unwrap();
    run(
        &db,
        "?[state, year, v] <- [['US', [100, true], 1], ['US', [2019, true], 2]] \
         :put hos {state, year => v}",
    )
    .unwrap();

    let rows = run(&db, "?[state, v] := *hos{state, v @ 250}").unwrap();
    assert_eq!(
        rows.as_array().unwrap().len(),
        1,
        "@ 250 (an abstract logical clock) must still work — see design-0122 §5"
    );
    let rows = run(&db, "?[state, v] := *hos{state, v @ 2019}").unwrap();
    assert_eq!(
        rows.as_array().unwrap().len(),
        1,
        "@ 2019 (the public tutorial's form) must still work"
    );
}

/// The spelling the new error messages RECOMMEND. It is a promise we make in a diagnostic, so
/// it gets a test, not just a doc line.
#[test]
fn control_recommended_to_int_spelling_works() {
    let db = seeded();
    let rows = run(
        &db,
        "?[k, v] := *ev{k, v @ to_int(parse_timestamp('2024-06-01T00:00:00Z') * 1000000)}",
    )
    .unwrap();
    assert_eq!(
        rows.as_array().unwrap().len(),
        1,
        "to_int(<seconds> * 1000000) is what the error tells people to write — it must work"
    );
}

/// Integer arithmetic in `@` still works; only float-*producing* ops now break (design-0122 §4.2a).
#[test]
fn control_integer_arithmetic_in_selector_works() {
    let db = seeded();
    let rows = run(
        &db,
        &format!("?[k, v] := *ev{{k, v @ ({MICROS_2024_06_01} + 0)}}"),
    )
    .unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 1);
}

// ═══════════════════════════ THE GAP WE DELIBERATELY DID NOT CLOSE ═══════════════════════════

/// CHARACTERIZATION, NOT A GUARANTEE. Seconds-as-an-*integer* is still silently accepted and
/// still returns nothing — `1717200000` is a `Num::Int`, so a Float rejection never sees it.
///
/// This is NOT fixable by a magnitude check: valid time is an abstract logical clock
/// (`control_small_abstract_ints_still_work` is the proof), so no threshold is legitimate.
/// The real answer is the TYPED path — `dt_to_validity` + `@ <Validity>` — in 0.13.0.
///
/// This test pins the known gap honestly. It should change only when 0.13.0 makes it
/// unnecessary. (Same convention as 0.12.1's honestly-labelled LSH pin.)
#[test]
fn known_gap_int_seconds_still_silently_returns_nothing() {
    let db = seeded();
    let rows = run(&db, "?[k, v] := *ev{k, v @ 1717200000}").unwrap();
    assert_eq!(
        rows.as_array().unwrap().len(),
        0,
        "int-seconds still silently reads 1970 — see design-0122 §5. If this ever changes, \
         the typed path landed and this pin should be retired, not 'fixed'."
    );
}
