/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! **T1 — the execution tier** of the planner regression suite. Nightly, not
//! per-PR (it runs for minutes). Design: `docs/plans/planner-regression-suite.md`.
//!
//! T0 (`planner_shape.rs`) proves the *plan* did not change. This tier proves the
//! plan is still *correct* and still *fast*, on real LDBC data with real skew —
//! the thing a synthetic uniform micro-bench structurally cannot show.
//!
//! ```sh
//! ./scripts/fetch_lsqb.sh                      # 6.1 MB, no generator
//! LSQB_SF01_DIR=.lsqb/social-network-sf0.1-projected-fk \
//!   cargo test --release -p mnestic --test lsqb -- --ignored --nocapture
//! ```
//!
//! `--release` is not optional: q1 takes ~63 s optimized and minutes in debug.
//! Whole tier: ~3.5 min (sqlite, Apple M-series). Measured 0.11.1 / sqlite:
//! load 1.5 s; q1 63.4 s, q2 50.3 s, q3 1.5 s, q6 33.1 s, q9 37.5 s.
//!
//! ## What is asserted, in order of trustworthiness
//!
//! 1. **Counts, against LSQB's own published expected-output** — an oracle
//!    authored by neither engine. A mismatch is a hard failure, never a warning.
//! 2. **The in-run reorder ratio on q3** — greedy vs `:reorder written`, both from
//!    the same binary in the same minute, so runner noise cancels by construction.
//!    Asserted as a bounded probe (`written` must NOT finish within 20× greedy)
//!    rather than by running the pathological plan out: measured, greedy is 1.4 s
//!    and written does not finish in 300 s, so paying for the full run buys a
//!    number, not a guarantee.
//! 3. **Wall-clock caps — a hang-guard, not the catcher.** See `cap_for`. The
//!    0.10.5 regression is caught by T0's plan-shape baseline; a cap alone would
//!    have missed it (with the reorder off, q3 still *completes*, just slowly), and
//!    a tight cap on the known-slow shapes would gate on runner speed rather than
//!    plan quality.

mod common;

use common::*;
use cozo::{DataValue, DbInstance, NamedRows, ScriptMutability};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Per-query wall-clock caps, and the reasoning is the point.
///
/// **q3 gets a real pathology cap (120 s).** It is the query the 0.10.5 regression
/// blew up, and its greedy plan runs in ~1.5 s here — an ~80× headroom that no
/// plausible runner erases. A breach means the plan went unbounded, full stop.
///
/// **Everything else gets a loose hang-guard (300 s), not a perf gate.** Measured
/// on sqlite: q1 63 s, q2 50 s, q6 33 s, q9 38 s. These are the shapes LSQB is
/// *publicly ceded* on — they are slow because the shape is hard, not because the
/// plan is bad. A 120 s cap on them would gate on how busy the runner is, which is
/// precisely the absolute-wall-clock brittleness this suite was built to avoid.
/// Their guard is the count oracle plus T0's plan-shape baseline.
fn cap_for(query_name: &str) -> Duration {
    match query_name {
        "lsqb_q3" => Duration::from_secs(120),
        _ => Duration::from_secs(300),
    }
}

/// The reorder must buy at least this much on q3. Asserted as a *bounded*
/// check — `written` is given `MIN_Q3_REORDER_RATIO ×` the greedy time and must
/// fail to finish in it — rather than by running the pathological plan to
/// completion, which costs minutes and establishes nothing T0 does not already
/// pin exactly. (Measured: greedy 1.4 s; written did not complete in 300 s, so
/// the true margin is >200×, not the ~128× first estimated on a quieter box.)
const MIN_Q3_REORDER_RATIO: u32 = 20;

/// Ceiling on the `written` probe, so a regression that slows the *greedy* arm
/// cannot make this test run away.
const CONTROL_PROBE_CAP_SECS: u64 = 90;

fn data_dir() -> PathBuf {
    let raw = std::env::var("LSQB_SF01_DIR").unwrap_or_else(|_| {
        panic!(
            "LSQB_SF01_DIR is not set.\n\
             Run `./scripts/fetch_lsqb.sh` and pass the path it prints, e.g.\n  \
             LSQB_SF01_DIR=.lsqb/social-network-sf0.1-projected-fk cargo test \
             --release -p mnestic --test lsqb -- --ignored"
        )
    });
    let path = PathBuf::from(raw);
    assert!(
        path.join("Person_knows_Person.csv").exists(),
        "{} does not look like an LSQB projected-fk directory (no Person_knows_Person.csv)",
        path.display()
    );
    path
}

/// Read a two-column LSQB edge CSV: pipe-delimited, one `:START_ID(X)|:END_ID(Y)`
/// header line, i64 ids (LDBC ids exceed 2^32 — parsing them as i32 truncates).
fn read_edges(dir: &Path, basename: &str) -> Vec<(i64, i64)> {
    let path = dir.join(format!("{basename}.csv"));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));

    text.lines()
        .skip(1) // header
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let (a, b) = line
                .split_once('|')
                .unwrap_or_else(|| panic!("malformed row in {basename}: {line:?}"));
            (
                a.trim()
                    .parse()
                    .unwrap_or_else(|e| panic!("{basename}: bad id {a:?}: {e}")),
                b.trim()
                    .parse()
                    .unwrap_or_else(|e| panic!("{basename}: bad id {b:?}: {e}")),
            )
        })
        .collect()
}

fn named_rows(edges: &[(i64, i64)], cols: Vec<String>) -> NamedRows {
    NamedRows::new(
        cols,
        edges
            .iter()
            .map(|(a, b)| vec![DataValue::from(*a), DataValue::from(*b)])
            .collect(),
    )
}

/// Load all 11 relations. Returns the loaded database.
///
/// **The symmetrization trap.** LSQB ships `Person_knows_Person` one-directionally
/// (18,135 rows, every row `src < dst`) while every LSQB query matches KNOWS
/// *undirected*. It must be materialized in both directions. Skip this and
/// q2/q3/q6/q9 — four of the five queries — return plausible, wrong numbers.
///
/// **The union-rule trap.** The symmetric form must be a **stored relation**, not
/// a union rule. The greedy reorder declines on any body containing a derived-rule
/// atom, so a rule-based KNOWS would make every query ineligible and the whole
/// suite would report green while guarding nothing. (`planner_shape.rs` asserts
/// this structurally via `every_gated_query_is_reorder_eligible`.)
fn load_lsqb(db: &DbInstance, dir: &Path) {
    for script in LSQB_RELATIONS {
        run_mut(db, script);
    }

    let mut payload: BTreeMap<String, NamedRows> = BTreeMap::new();

    for (relation, basename) in LSQB_CSV_MAP {
        let mut edges = read_edges(dir, basename);

        if *relation == "knows" {
            assert_eq!(
                edges.len(),
                KNOWS_RAW_ROWS,
                "LSQB shipped {} KNOWS rows, expected {KNOWS_RAW_ROWS} — the dataset changed \
                 under the pinned sha256, and the expected counts no longer apply",
                edges.len()
            );
            let reversed: Vec<(i64, i64)> = edges.iter().map(|(a, b)| (*b, *a)).collect();
            edges.extend(reversed);
            assert_eq!(edges.len(), KNOWS_SYMMETRIC_ROWS, "symmetrization is wrong");
        }

        payload.insert(
            relation.to_string(),
            named_rows(&edges, schema_cols(relation)),
        );
    }

    db.import_relations(payload)
        .expect("import_relations failed");
}

/// The declared column names for a relation, taken from `LSQB_RELATIONS` so the
/// loader can never drift from the schema the plan-shape gate pinned.
fn schema_cols(relation: &str) -> Vec<String> {
    let decl = LSQB_RELATIONS
        .iter()
        .find(|s| s.starts_with(&format!(":create {relation} ")))
        .unwrap_or_else(|| panic!("no schema for {relation}"));
    let inner = decl
        .split_once('{')
        .and_then(|(_, r)| r.rsplit_once('}'))
        .expect("malformed :create")
        .0;
    inner
        .split(',')
        .map(|f| f.split(':').next().unwrap().trim().to_string())
        .collect()
}

struct Timed {
    count: i64,
    elapsed: Duration,
}

/// Run a counting query under a wall-clock cap. `None` means it breached the cap.
fn try_timed_count(db: &DbInstance, query: &str, cap: Duration) -> Option<Timed> {
    let script = format!("{query}\n:timeout {}", cap.as_secs());
    let start = Instant::now();
    match db.run_script(&script, BTreeMap::new(), ScriptMutability::Immutable) {
        Ok(rows) => Some(Timed {
            count: rows.rows[0][0]
                .get_int()
                .expect("counting query did not return an integer"),
            elapsed: start.elapsed(),
        }),
        Err(_) => None,
    }
}

/// Run a counting query that is *expected* to finish inside the cap. Breaching
/// it is the pathology signal.
fn timed_count(db: &DbInstance, query: &str, cap: Duration) -> Timed {
    try_timed_count(db, query, cap).unwrap_or_else(|| {
        panic!(
            "query breached its {}s pathology cap — the planner produced something \
             unbounded. This is the 0.10.5 class of failure.\n--- script ---\n{query}",
            cap.as_secs()
        )
    })
}

// ---------------------------------------------------------------------------

/// Every ported LSQB query must reproduce LSQB's own published sf0.1 count, and
/// no greedy plan may breach the pathology cap.
#[test]
#[ignore = "needs the LSQB sf0.1 dataset; run nightly via scripts/fetch_lsqb.sh"]
fn lsqb_counts_match_the_published_oracle() {
    let dir = data_dir();
    let tmp = tempfile::tempdir().unwrap();
    let db = DbInstance::new(
        "sqlite",
        tmp.path().join("lsqb.db").to_str().unwrap(),
        Default::default(),
    )
    .unwrap();

    let t0 = Instant::now();
    load_lsqb(&db, &dir);
    println!("loaded LSQB sf0.1 in {:?}", t0.elapsed());

    let oracle: BTreeMap<&str, i64> = LSQB_ORACLE.iter().copied().collect();

    for (name, query) in LSQB_QUERIES {
        let got = timed_count(&db, query, cap_for(name));
        let want = oracle[name];
        println!(
            "{name:>8}  {:>9.1} ms  count={}",
            got.elapsed.as_secs_f64() * 1000.0,
            got.count
        );

        assert_eq!(
            got.count, want,
            "{name} returned {} but LSQB's published sf0.1 expected-output is {want}. \
             This is a CORRECTNESS failure, not a performance one. The usual cause is \
             the KNOWS symmetrization (18,135 -> 36,270) having been dropped.",
            got.count
        );
    }

    // mnestic fork (0.13.0): q6 again with the factorized-count rewrite forced
    // ON. Without this arm the whole tier exercises only the default-OFF path,
    // and a broken `!=` inclusion–exclusion would ship behind a green gate (the
    // toggle is not reachable from CI config — only from test code). The oracle
    // is toggle-agnostic: 55,607,896 is correct whether or not the rewrite
    // fires. Measured 2026-07-17 (sqlite, M-series, release): OFF ~41.7 s,
    // ON ~0.30 s — ~140×.
    db.set_query_factorization(true);
    // The count alone cannot distinguish "rewrite fired and is exact" from
    // "rewrite silently declined and this is a second naive run" — assert the
    // plan actually contains the synthesized `fac*` rules, so a gate/extract
    // regression that declines q6 goes RED here instead of hiding behind a
    // green (but 140× slower) arm.
    let plan = db
        .run_script(
            &format!("::explain {{ {Q6} }}"),
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .expect("::explain failed");
    let rule_idx = plan
        .headers
        .iter()
        .position(|h| h == "rule")
        .expect("::explain output has a 'rule' column");
    // The synthesized helper rules are `*fac…`-named (the same signal
    // tests/factorize.rs's `fired()` pins).
    let fired = plan
        .rows
        .iter()
        .any(|r| r[rule_idx].get_str().is_some_and(|s| s.starts_with("*fac")));
    assert!(
        fired,
        "the factorized-count rewrite did not fire on q6 with the toggle ON — \
         the type gate or extractor declined a shape it must accept"
    );
    let on = timed_count(&db, Q6, cap_for("lsqb_q6"));
    db.set_query_factorization(false);
    println!(
        "  q6(on)  {:>9.1} ms  count={}",
        on.elapsed.as_secs_f64() * 1000.0,
        on.count
    );
    assert_eq!(
        on.count, oracle["lsqb_q6"],
        "q6 with the factorized-count rewrite forced ON miscounts — the `!=` \
         inclusion–exclusion is wrong (this is the a60a8013 class of failure)"
    );
}

/// The reorder must still be worth having on q3 — the shape it was written for.
///
/// Both arms run in the same process, so this ratio is immune to the 2.5–3×
/// runner noise that makes any cross-version millisecond threshold flaky. The
/// 0.10.5 regression inverts it outright: the greedy arm becomes the slow one.
#[test]
#[ignore = "needs the LSQB sf0.1 dataset; run nightly via scripts/fetch_lsqb.sh"]
fn q3_reorder_ratio_holds() {
    let dir = data_dir();
    let tmp = tempfile::tempdir().unwrap();
    let db = DbInstance::new(
        "sqlite",
        tmp.path().join("lsqb.db").to_str().unwrap(),
        Default::default(),
    )
    .unwrap();
    load_lsqb(&db, &dir);

    let greedy = timed_count(&db, Q3, cap_for("lsqb_q3"));

    // Give the written order a budget of K x what greedy took. It must NOT
    // finish in it. Bounding the probe this way keeps the assertion honest
    // (it is a real ratio) without paying to run a pathological plan to
    // completion, and it degrades safely: if greedy itself regresses, the
    // budget shrinks toward the ceiling rather than running away.
    let budget = (greedy.elapsed * MIN_Q3_REORDER_RATIO)
        .min(Duration::from_secs(CONTROL_PROBE_CAP_SECS))
        .max(Duration::from_secs(1)); // `:timeout` has 1s granularity

    let written = try_timed_count(&db, &format!("{Q3}\n:reorder written"), budget);

    println!(
        "q3  greedy {:.1} ms | written given {:.0}s ({}x greedy) -> {}",
        greedy.elapsed.as_secs_f64() * 1000.0,
        budget.as_secs_f64(),
        MIN_Q3_REORDER_RATIO,
        match &written {
            Some(t) => format!("finished in {:.1} ms", t.elapsed.as_secs_f64() * 1000.0),
            None => "did not finish (expected)".to_string(),
        }
    );

    if let Some(w) = written {
        // If it finished, the answer had better still be right — but either way
        // the reorder has stopped being worth having on the shape it exists for.
        assert_eq!(greedy.count, w.count, "the reorder changed the ANSWER");
        panic!(
            "q3's written order finished in {:.1} ms, within {}x the greedy plan's \
             {:.1} ms. The reorder is no longer buying anything on the very shape it \
             was written for — either it stopped firing, or its plan is now no better \
             than written order (the 0.10.5 class of regression).",
            w.elapsed.as_secs_f64() * 1000.0,
            MIN_Q3_REORDER_RATIO,
            greedy.elapsed.as_secs_f64() * 1000.0,
        );
    }
}
