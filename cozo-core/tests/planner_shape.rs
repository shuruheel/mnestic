/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! **T0 — the planner regression gate.** Runs on every PR, against HEAD, with
//! no dataset, in milliseconds. Design: `docs/plans/planner-regression-suite.md`.
//!
//! ## What this exists to catch
//!
//! 0.10.5 shipped a default-on greedy join reorder whose tie-break scored a
//! *partial* composite-key prefix as if it were a point lookup, so on LSQB q3 it
//! pulled a keyed fan-out expansion ahead of a selective atom. The query went
//! from ~19 s to a >120 s timeout. **A field-tester found it; our tests did
//! not.** It was fixed in 0.10.7.
//!
//! ## Why this is a *plan-shape* gate and not a benchmark
//!
//! Three facts, each measured against the 0.10.5 and 0.11.1 wheels:
//!
//! 1. **The reorder is stat-free.** It plans from the schema, never from
//!    cardinality, so `::explain` over *empty* relations returns the
//!    byte-identical plan to the fully-loaded 2M-row dataset. The gate needs no
//!    data at all.
//! 2. **Wall-clock would not have caught it.** q3 with the reorder *off*
//!    completes in 99.4 s — under any sane 120 s pathology cap, correct answer,
//!    just slow. A timing gate sails through on a fast runner.
//! 3. **The synthetic triangle repro would not have caught it either.** Its
//!    `load_refs` is byte-identical on 0.10.5 and 0.11.1 (in that fixture every
//!    tied candidate has its leading key column bound, so all score the same and
//!    the buggy tie-break never decides anything). It guards that the reorder
//!    still *helps*; it cannot see the reorder *hurting*. See `join_reorder.rs`.
//!
//! What is left is the join order itself, pinned against a committed baseline.
//! On 0.10.5 vs 0.11.1, q3's `load_refs` differ — that difference is the gate.
//!
//! ## The contract with a reviewer
//!
//! A plan-shape change is **not necessarily a regression** — an intentional
//! planner improvement also changes it. This test is a *"a human must look"*
//! tripwire, not a verdict. When it fires legitimately, refresh the baseline
//! (`cargo test -p mnestic --test planner_shape -- --ignored regenerate`) **in
//! the same PR**, and confirm against the T1 execution tier (`lsqb.rs`) that the
//! new plan is not slower. CI never writes a baseline: a failing run must not be
//! able to launder itself green.

mod common;

use common::*;
use cozo::DbInstance;
use serde_json::{json, Map, Value};

/// The committed plan-shape baseline. `include_str!` binds it at compile time,
/// so the gate cannot silently pass against a missing file.
const BASELINE_JSON: &str = include_str!("planner_baseline.json");

fn baseline() -> Map<String, Value> {
    let v: Value =
        serde_json::from_str(BASELINE_JSON).expect("planner_baseline.json is not valid JSON");
    v.get("queries")
        .and_then(|q| q.as_object())
        .expect("planner_baseline.json has no `queries` object")
        .clone()
}

fn expected(base: &Map<String, Value>, name: &str, arm: &str) -> Vec<String> {
    base.get(name)
        .and_then(|q| q.get(arm))
        .and_then(|a| a.as_array())
        .unwrap_or_else(|| panic!("planner_baseline.json is missing queries.{name}.{arm}"))
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_string())
        .collect()
}

fn lsqb_db(dir: &tempfile::TempDir) -> DbInstance {
    empty_lsqb_db(&dir.path().join("shape.db"))
}

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

/// **THE GATE.** Every ported LSQB query's join order must match its committed
/// signature, under both the greedy planner and the `written` control.
///
/// This is the assertion that goes RED on 0.10.5 and green on 0.11.1. That was
/// verified by hand, by running this test against the `v0.10.5` tag in a
/// worktree — it is a one-off negative control, NOT a CI job. `planner-guard.yml`
/// runs only the nightly LSQB execution tier; nothing re-checks the control.
#[test]
fn lsqb_join_order_matches_committed_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let db = lsqb_db(&dir);
    let base = baseline();

    let mut drifted = vec![];
    for (name, query) in LSQB_QUERIES {
        for (arm, got) in [
            ("greedy", greedy_refs(&db, query)),
            ("written", written_refs(&db, query)),
        ] {
            let want = expected(&base, name, arm);
            if got != want {
                drifted.push(format!(
                    "\n  {name} [{arm}]\n    baseline: {want:?}\n    current:  {got:?}"
                ));
            }
        }
    }

    assert!(
        drifted.is_empty(),
        "PLANNER PLAN-SHAPE DRIFT — the join order changed for:{}\n\n\
         This is a tripwire, not a verdict. Either:\n\
         (a) you did not mean to change the planner — this is the 0.10.5 class of \
         regression, and it is exactly what this gate exists to catch; or\n\
         (b) you meant to. Then confirm the new plan is not SLOWER by running the \
         T1 execution tier (`cargo test --release -p mnestic --test lsqb -- --ignored`), \
         and refresh the baseline in this same PR:\n\
         `cargo test -p mnestic --test planner_shape -- --ignored regenerate`\n",
        drifted.join("")
    );
}

/// The pass must still *fire* on q3. Guards the failure mode where the reorder
/// silently stops running (an eligibility bug, a bad early return) — the
/// baseline alone cannot see that, because a disabled pass and a correct pass
/// could in principle agree.
///
/// Both arms come from the same binary in the same process, so unlike the
/// committed baseline this assertion cannot rot across versions.
#[test]
fn reorder_still_fires_on_the_q3_triangle() {
    let dir = tempfile::tempdir().unwrap();
    let db = lsqb_db(&dir);

    assert_ne!(
        greedy_refs(&db, Q3),
        written_refs(&db, Q3),
        "the greedy reorder no longer changes q3's join order — the pass has \
         stopped firing on the very shape it was written for"
    );
}

/// Every gated query must actually reach the reorder pass.
///
/// This guards the most dangerous silent failure available to this suite: the
/// pass **declines on any body containing a derived-rule atom**. Express the
/// symmetric `knows` (or LSQB's `:Message` union) as a union *rule* instead of a
/// *stored* relation and every query becomes ineligible — the suite would report
/// all-green while guarding nothing at all.
#[test]
fn every_gated_query_is_reorder_eligible() {
    let dir = tempfile::tempdir().unwrap();
    let db = lsqb_db(&dir);

    for (name, query) in LSQB_QUERIES {
        let n = greedy_refs(&db, query).len();
        assert!(
            n >= 3,
            "{name} loads only {n} stored relation(s); the reorder pass requires \
             ≥3 positive stored atoms in a body, so this query is guarding NOTHING. \
             The usual cause is a relation having been made a derived rule."
        );
    }
}

/// Plan shape over stored relations is decided by the `RelAlgebra` variant, not
/// by the storage backend (`Stored` → `stored_*_join` on every backend). Pinning
/// that here means the gate can run on the cheapest backend without qualification
/// — and documents why, since the repo's test-backend rule (`mem` uses a separate
/// join operator) is about stored-path *correctness*, not plan shape.
#[test]
fn plan_shape_is_backend_independent() {
    let dir = tempfile::tempdir().unwrap();
    let sqlite = lsqb_db(&dir);

    let mem = DbInstance::new("mem", "", Default::default()).unwrap();
    for script in LSQB_RELATIONS {
        run_mut(&mem, script);
    }

    for (name, query) in LSQB_QUERIES {
        assert_eq!(
            greedy_refs(&sqlite, query),
            greedy_refs(&mem, query),
            "{name}: plan shape differs between the sqlite and mem backends"
        );
    }
}

// ---------------------------------------------------------------------------
// Baseline regeneration — human-only, never CI
// ---------------------------------------------------------------------------

/// Rewrite `planner_baseline.json` from the current engine.
///
/// `#[ignore]`d so CI can never run it: a failing gate must not be able to
/// launder itself into a passing baseline. Refresh deliberately, in a reviewed
/// commit, with the diff visible — the diff *is* the thing being approved.
///
/// ```sh
/// cargo test -p mnestic --test planner_shape -- --ignored regenerate
/// ```
#[test]
#[ignore = "human-only: rewrites the committed baseline"]
fn regenerate_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let db = lsqb_db(&dir);

    let mut queries = Map::new();
    for (name, query) in LSQB_QUERIES {
        queries.insert(
            name.to_string(),
            json!({
                "greedy": greedy_refs(&db, query),
                "written": written_refs(&db, query),
            }),
        );
    }

    let doc = json!({
        "_comment": "Committed plan-shape baseline for the T0 planner gate \
                     (cozo-core/tests/planner_shape.rs). The ordered `load_stored` \
                     relation sequence per query — i.e. the join order. Regenerate \
                     with: cargo test -p mnestic --test planner_shape -- --ignored \
                     regenerate. Refresh ONLY in a reviewed commit that explains why \
                     the plan changed; CI never writes this file.",
        "engine_version": env!("CARGO_PKG_VERSION"),
        "queries": queries,
    });

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("planner_baseline.json");
    let mut out = serde_json::to_string_pretty(&doc).unwrap();
    out.push('\n');
    std::fs::write(&path, out).unwrap();
    println!("wrote {}", path.display());
}
