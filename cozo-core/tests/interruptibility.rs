/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Interruptibility regression tests (mnestic fork, 0.10.5).
//!
//! Pins the two engine fixes surfaced by an external Ladybug-vs-mnestic
//! benchmark:
//!
//! * **Fix A — non-blocking `::running` / `::kill`.** These sysops touch only
//!   the in-memory running-query registry, so they now dispatch *before* the
//!   single-transaction wrapper. Previously a `::kill` opened a write tx first,
//!   which on the mem/sqlite backends queued behind the very read query it was
//!   trying to kill — so the kill blocked for that query's entire remaining
//!   runtime.
//!
//! * **Fix B — poison checks fire inside RA enumeration.** A naive single-rule
//!   join written in a pathological order enumerates a combinatorial
//!   intermediate lazily inside one `Iterator::next()`, yielding no output for a
//!   long time. The poison flag (set by `::kill` and by the `:timeout` query
//!   option) is now checked every `POISON_CHECK_INTERVAL` pulls at every
//!   operator boundary and raw scan, so a kill/timeout aborts within a bounded
//!   amount of work instead of after the full enumeration completes.
//!
//! The workload is the benchmark's "same-group triangle": `member(c, p)` places
//! N people in one group; `knows(a, b)` is a tiny clique. The triangle query is
//! written members-first, so the selective `knows` atoms are applied last — the
//! planner would (without a kill) enumerate ~N^3 same-group people-triples. The
//! query is uninteresting; the point is that it runs long enough to interrupt.

use cozo::{DataValue, DbInstance, NamedRows, ScriptMutability};
use std::collections::BTreeMap;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// People in the single group. N^3 candidate triples must dominate the
/// timeout/kill latency on any machine: at N=280 that is ~22M triples, tens of
/// seconds of debug-build enumeration, versus the ~1s interruption we assert.
const N: i64 = 280;

/// The pathological members-first triangle (selective `knows` atoms last).
const SPIN_QUERY: &str = "?[count(x)] := *member[c, a], *member[c, b], *member[c, d], \
     *knows[a, b], *knows[b, d], *knows[d, a], x = [a, b, d]";

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
    // A small bidirectional clique on 0..12, so real directed triangles exist
    // and the query yields a (small) count if ever run to completion.
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

/// Fix B via the `:timeout` query option, on a backend whose join path this
/// exercises. Without in-enumeration poison checks this query runs to
/// completion (tens of seconds); with them it aborts ~1s after the deadline.
fn assert_timeout_aborts(engine: &str, path: &str) {
    let db = setup_spin_db(engine, path);
    let script = format!("{SPIN_QUERY} :timeout 1");

    let t0 = Instant::now();
    let res = db.run_script(&script, BTreeMap::new(), ScriptMutability::Immutable);
    let elapsed = t0.elapsed();

    assert!(
        res.is_err(),
        "[{engine}] `:timeout 1` on the spin query should error, got Ok"
    );
    assert!(
        elapsed < Duration::from_secs(6),
        "[{engine}] `:timeout 1` aborted after {elapsed:?}; poison is not being \
         checked inside RA enumeration (fix B regressed)"
    );
}

#[test]
fn timeout_aborts_long_join_mem() {
    assert_timeout_aborts("mem", "");
}

#[test]
fn timeout_aborts_long_join_sqlite() {
    // sqlite exercises the real stored_* join path (the mem backend uses a
    // separate operator), per tests/matjoin_regression.rs.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mnestic_interrupt.db");
    assert_timeout_aborts("sqlite", path.to_str().unwrap());
}

/// Fix A (prompt `::running`/`::kill` dispatch) + fix B (the kill actually
/// aborts the enumeration). One thread spins on the query; the main thread
/// introspects and kills it.
#[test]
fn kill_interrupts_running_query_mem() {
    let db = setup_spin_db("mem", "");
    let worker_db = db.clone();

    let (tx, rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let t0 = Instant::now();
        let res = worker_db.run_script(SPIN_QUERY, BTreeMap::new(), ScriptMutability::Immutable);
        let _ = tx.send((res.is_err(), t0.elapsed()));
    });

    // Let the query register and get into the enumeration.
    thread::sleep(Duration::from_millis(500));

    // Fix A: ::running must return promptly and list the in-flight query.
    let t_run = Instant::now();
    let running = db
        .run_script("::running", BTreeMap::new(), ScriptMutability::Immutable)
        .expect("::running failed");
    assert!(
        t_run.elapsed() < Duration::from_millis(500),
        "::running blocked for {:?} behind the running query (fix A regressed)",
        t_run.elapsed()
    );
    assert!(
        !running.rows.is_empty(),
        "the spinning query was not listed by ::running"
    );
    let id = running.rows[0][0]
        .get_int()
        .expect("::running id column should be an int");

    // Fix A: even a MUTABLE ::kill (the worst case — it used to open a write tx
    // that queued behind the reader) must return promptly.
    let t_kill = Instant::now();
    let killed = db
        .run_script(
            &format!("::kill {id}"),
            BTreeMap::new(),
            ScriptMutability::Mutable,
        )
        .expect("::kill failed");
    assert!(
        t_kill.elapsed() < Duration::from_millis(500),
        "::kill blocked for {:?} behind the target query (fix A regressed)",
        t_kill.elapsed()
    );
    assert_eq!(
        killed.rows[0][0],
        DataValue::from("KILLING"),
        "::kill on a live query should report KILLING, got {:?}",
        killed.rows[0][0]
    );

    // Fix B: the worker aborts soon after the poison is set, rather than running
    // the enumeration to completion.
    let (was_err, worker_elapsed) = rx
        .recv_timeout(Duration::from_secs(6))
        .expect("worker did not finish within 6s of the kill (fix B regressed)");
    assert!(
        was_err,
        "the killed query returned Ok after {worker_elapsed:?} — it ran to \
         completion instead of aborting (fix B regressed)"
    );

    worker.join().unwrap();
}
