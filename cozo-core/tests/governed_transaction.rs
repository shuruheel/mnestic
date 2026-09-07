/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

#![cfg(not(target_arch = "wasm32"))]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cozo::{
    DataValue, DbInstance, GovernedTransaction, GovernedTransactionOptions, NamedRows,
    ScriptMutability, SimpleFixedRule,
};
use crossbeam::channel::bounded;
use serde_json::json;

fn fixture(engine: &str) -> (tempfile::TempDir, DbInstance) {
    let dir = tempfile::tempdir().unwrap();
    let db = DbInstance::new(engine, dir.path().join("db").to_str().unwrap(), "").unwrap();
    run(&db, ":create items { id: Int => value: Int }");
    run(&db, "?[id, value] <- [[1, 10]] :put items {id => value}");
    (dir, db)
}

fn run(db: &DbInstance, script: &str) -> NamedRows {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap()
}

fn options() -> GovernedTransactionOptions {
    GovernedTransactionOptions::new(Instant::now() + Duration::from_secs(10))
}

fn start(
    db: &DbInstance,
    write: bool,
    options: GovernedTransactionOptions,
) -> (GovernedTransaction, JoinHandle<()>) {
    let (client, worker) = db.governed_transaction(write, options).unwrap();
    (client, thread::spawn(move || worker.run()))
}

fn code(error: miette::Report) -> String {
    error.code().expect("typed diagnostic").to_string()
}

fn committed_and_aborted_writes(engine: &str) {
    let (_dir, db) = fixture(engine);
    let (mut tx, worker) = start(&db, true, options());
    tx.run_script(
        "?[id, value] <- [[2, 20]] :put items {id => value}",
        BTreeMap::new(),
    )
    .unwrap();
    // Running-query cleanup must not kill the parent after the first query.
    tx.run_script(
        "?[id, value] <- [[3, 30]] :put items {id => value}",
        BTreeMap::new(),
    )
    .unwrap();
    tx.commit().unwrap();
    worker.join().unwrap();
    assert_eq!(
        run(&db, "?[id, value] := *items{id, value}").into_json()["rows"],
        json!([[1, 10], [2, 20], [3, 30]])
    );

    let (mut tx, worker) = start(&db, true, options());
    tx.run_script(
        "?[id, value] <- [[4, 40]] :put items {id => value}",
        BTreeMap::new(),
    )
    .unwrap();
    tx.abort().unwrap();
    worker.join().unwrap();
    assert_eq!(run(&db, "?[id] := *items{id}, id == 4").rows.len(), 0);
}

fn read_only_and_error_abort(engine: &str) {
    let (_dir, db) = fixture(engine);
    let (mut tx, worker) = start(&db, false, options());
    let error = tx
        .run_script(
            "?[id, value] <- [[2, 20]] :put items {id => value}",
            BTreeMap::new(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("read-only transaction"));
    worker.join().unwrap();
    assert_eq!(
        code(tx.run_script("?[x] <- [[1]]", BTreeMap::new()).unwrap_err()),
        "transaction::closed"
    );

    let (mut tx, worker) = start(&db, true, options());
    tx.run_script(
        "?[id, value] <- [[2, 20]] :put items {id => value}",
        BTreeMap::new(),
    )
    .unwrap();
    assert!(tx.run_script("invalid query [", BTreeMap::new()).is_err());
    worker.join().unwrap();
    assert!(tx.commit().is_err());
    assert_eq!(
        run(&db, "?[id, value] := *items{id, value}").into_json()["rows"],
        json!([[1, 10]])
    );
}

fn memory_minimum_and_rollback(engine: &str) {
    let (_dir, db) = fixture(engine);
    run(&db, ":create base { x: Int }");
    db.import_relations(BTreeMap::from([(
        "base".into(),
        NamedRows::new(
            vec!["x".into()],
            (0..200).map(|i| vec![DataValue::from(i)]).collect(),
        ),
    )]))
    .unwrap();
    // Each source must independently tighten the others; no single-setting
    // test can distinguish accidental ignoring of the Db or per-call limit.
    for tight in 0..3 {
        db.set_default_query_mem_limit(Some(if tight == 0 { 100_000 } else { 100_000_000 }));
        let mut opts = options();
        opts.mem_limit = Some(if tight == 1 { 100_000 } else { 100_000_000 });
        let (mut tx, worker) = start(&db, true, opts);
        tx.run_script(
            "?[id, value] <- [[2, 20]] :put items {id => value}",
            BTreeMap::new(),
        )
        .unwrap();
        let block_limit = if tight == 2 { 100_000 } else { 100_000_000 };
        let error = tx.run_script(&format!("tmp[a,b,c] := *base[a], *base[b], *base[c]\n?[count(a)] := tmp[a,_,_] :mem_limit {block_limit}"), BTreeMap::new()).unwrap_err();
        assert_eq!(
            code(error),
            "eval::mem_budget_exceeded",
            "{engine} limit source {tight}"
        );
        worker.join().unwrap();
        assert!(tx.commit().is_err());
        assert_eq!(
            run(&db, "?[id, value] := *items{id, value}").into_json()["rows"],
            json!([[1, 10]])
        );
    }
    let warnings = run(&db, "::warnings");
    assert!(
        warnings
            .rows
            .iter()
            .any(|row| row[1].get_str().is_some_and(|c| c.contains("cartesian"))),
        "warnings from failed evaluation must be visible"
    );
}

fn idle_timeout_retains_diagnostic_and_rolls_back(engine: &str) {
    let (_dir, db) = fixture(engine);
    let mut opts = options();
    opts.idle_timeout = Duration::from_millis(100);
    let (mut tx, worker) = start(&db, true, opts);
    tx.run_script(
        "?[id, value] <- [[2, 20]] :put items {id => value}",
        BTreeMap::new(),
    )
    .unwrap();
    worker.join().unwrap();
    // A failed command send after the worker exited still exposes its timeout.
    assert_eq!(
        code(tx.run_script("?[x] <- [[1]]", BTreeMap::new()).unwrap_err()),
        "eval::timeout"
    );
    assert_eq!(
        run(&db, "?[id, value] := *items{id, value}").into_json()["rows"],
        json!([[1, 10]])
    );
}

fn whole_deadline_and_bounded_sleep(engine: &str) {
    let (_dir, db) = fixture(engine);
    for source in 0..3 {
        let mut opts = options();
        if source == 0 {
            opts.deadline = Instant::now() + Duration::from_millis(100);
        }
        db.set_default_query_timeout((source == 1).then_some(0.1));
        let (mut tx, worker) = start(&db, true, opts);
        tx.run_script(
            "?[id, value] <- [[2, 20]] :put items {id => value}",
            BTreeMap::new(),
        )
        .unwrap();
        let started = Instant::now();
        let query = if source == 2 {
            "?[x] <- [[1]] :sleep 5 :timeout 0.1"
        } else {
            "?[x] <- [[1]] :sleep 5"
        };
        assert_eq!(
            code(tx.run_script(query, BTreeMap::new()).unwrap_err()),
            "eval::timeout"
        );
        worker.join().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "sleep must not retain a worker for five seconds"
        );
        assert_eq!(
            run(&db, "?[id, value] := *items{id, value}").into_json()["rows"],
            json!([[1, 10]])
        );
    }
}

fn disconnect_rolls_back(engine: &str) {
    let (_dir, db) = fixture(engine);
    let (mut tx, worker) = start(&db, true, options());
    tx.run_script(
        "?[id, value] <- [[2, 20]] :put items {id => value}",
        BTreeMap::new(),
    )
    .unwrap();
    drop(tx);
    worker.join().unwrap();
    assert_eq!(
        run(&db, "?[id, value] := *items{id, value}").into_json()["rows"],
        json!([[1, 10]])
    );
}

fn warnings_visible_before_write_transaction_closes(engine: &str) {
    let (_dir, db) = fixture(engine);
    let (mut tx, worker) = start(&db, true, options());
    tx.run_script(
        "?[a,b,c] := *items{id:a}, *items{id:b}, *items{id:c}",
        BTreeMap::new(),
    )
    .unwrap();
    assert!(run(&db, "::warnings")
        .rows
        .iter()
        .any(|row| row[1].get_str() == Some("query.cartesian_step")));
    run(&db, "::warnings clear");
    assert!(run(&db, "::warnings").rows.is_empty());
    // These sysops must not wait for the active transaction's idle timeout.
    tx.run_script("?[x] <- [[1]]", BTreeMap::new()).unwrap();
    tx.abort().unwrap();
    worker.join().unwrap();
}

#[cfg(feature = "test-hooks")]
fn commit_failure_publishes_no_changes(engine: &str) {
    let (_dir, db) = fixture(engine);
    let (_, callback) = db.register_callback("items", None);
    let (mut tx, worker) = start(&db, true, options());
    tx.run_script(
        "?[id, value] <- [[2, 20]] :put items {id => value}",
        BTreeMap::new(),
    )
    .unwrap();
    db.fail_next_commit_for_tests();
    assert!(tx
        .commit()
        .unwrap_err()
        .to_string()
        .contains("injected commit failure"));
    worker.join().unwrap();
    assert!(callback.try_recv().is_err());
    assert_eq!(
        run(&db, "?[id, value] := *items{id, value}").into_json()["rows"],
        json!([[1, 10]])
    );
}

fn snapshot_survives_concurrent_write(engine: &str) {
    let (_dir, db) = fixture(engine);
    let (mut tx, worker) = start(&db, false, options());
    let before = tx
        .run_script("?[id, value] := *items{id, value}", BTreeMap::new())
        .unwrap()
        .into_json();
    let db2 = db.clone();
    let (attempt_tx, attempt_rx) = bounded(1);
    let (done_tx, done_rx) = bounded(1);
    let writer = thread::spawn(move || {
        attempt_tx.send(()).unwrap();
        run(&db2, "?[id, value] <- [[1, 99]] :put items {id => value}");
        done_tx.send(()).unwrap();
    });
    attempt_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    if engine == "rocksdb" {
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    } else {
        // SQLite holds a store-wide read guard. The concurrent writer waits
        // until this reader closes instead of committing alongside it.
        assert!(done_rx.recv_timeout(Duration::from_millis(100)).is_err());
    }
    assert_eq!(
        tx.run_script("?[id, value] := *items{id, value}", BTreeMap::new())
            .unwrap()
            .into_json(),
        before
    );
    tx.abort().unwrap();
    worker.join().unwrap();
    writer.join().unwrap();
    assert_eq!(
        run(&db, "?[id, value] := *items{id, value}").into_json()["rows"],
        json!([[1, 99]])
    );
}

macro_rules! matrix {
    ($module:ident, $engine:literal) => {
        mod $module {
            use super::*;
            #[test]
            fn commit_and_abort() {
                committed_and_aborted_writes($engine);
            }
            #[test]
            fn read_only_and_failure() {
                read_only_and_error_abort($engine);
            }
            #[test]
            fn memory() {
                memory_minimum_and_rollback($engine);
            }
            #[test]
            fn idle() {
                idle_timeout_retains_diagnostic_and_rolls_back($engine);
            }
            #[test]
            fn deadlines() {
                whole_deadline_and_bounded_sleep($engine);
            }
            #[test]
            fn disconnect() {
                disconnect_rolls_back($engine);
            }
            #[test]
            fn warnings_while_open() {
                warnings_visible_before_write_transaction_closes($engine);
            }
            #[cfg(feature = "test-hooks")]
            #[test]
            fn failed_commit() {
                commit_failure_publishes_no_changes($engine);
            }
        }
    };
}

matrix!(mem, "mem");
#[cfg(feature = "storage-sqlite")]
matrix!(sqlite, "sqlite");
#[cfg(feature = "storage-rocksdb")]
matrix!(rocks, "rocksdb");

#[cfg(feature = "storage-sqlite")]
#[test]
fn sqlite_snapshot() {
    snapshot_survives_concurrent_write("sqlite");
}

#[cfg(feature = "storage-rocksdb")]
#[test]
fn rocks_snapshot() {
    snapshot_survives_concurrent_write("rocksdb");
}

#[test]
fn cancellation_keeps_permit_on_actual_worker_until_uncooperative_code_exits() {
    let (_dir, db) = fixture("mem");
    let (entered_tx, entered_rx) = bounded(1);
    let (release_tx, release_rx) = bounded(1);
    db.register_fixed_rule(
        "Hold".into(),
        SimpleFixedRule::new(1, move |_, _| {
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(NamedRows::new(
                vec!["x".into()],
                vec![vec![DataValue::from(1)]],
            ))
        }),
    )
    .unwrap();
    struct Permit(Arc<AtomicBool>);
    impl Drop for Permit {
        fn drop(&mut self) {
            self.0.store(false, Ordering::SeqCst);
        }
    }
    let held = Arc::new(AtomicBool::new(true));
    let permit = Permit(held.clone());
    let mut opts = options();
    opts.deadline = Instant::now() + Duration::from_secs(1);
    let (mut client, worker) = db.governed_transaction(false, opts).unwrap();
    let cancellation = client.cancellation();
    let worker = thread::spawn(move || {
        worker.run();
        drop(permit);
    });
    let caller = thread::spawn(move || client.run_script("?[x] <~ Hold()", BTreeMap::new()));
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    cancellation.cancel();
    assert!(caller.join().unwrap().is_err());
    assert!(
        held.load(Ordering::SeqCst),
        "the client returned but its worker is still in host code"
    );
    release_tx.send(()).unwrap();
    worker.join().unwrap();
    assert!(!held.load(Ordering::SeqCst));
}

#[test]
fn scheduling_delay_counts_and_zero_idle_is_rejected() {
    let (_dir, db) = fixture("mem");
    let mut opts = options();
    opts.idle_timeout = Duration::ZERO;
    assert!(db.governed_transaction(false, opts).is_err());
    let opts = GovernedTransactionOptions::new(Instant::now() - Duration::from_millis(1));
    let (mut tx, worker) = start(&db, false, opts);
    assert_eq!(
        code(tx.run_script("?[x] <- [[1]]", BTreeMap::new()).unwrap_err()),
        "eval::timeout"
    );
    worker.join().unwrap();
}

#[test]
fn stalled_callback_does_not_hold_committed_worker_forever() {
    let (_dir, db) = fixture("mem");
    let (_, callback) = db.register_callback("items", Some(0));
    let mut opts = options();
    opts.deadline = Instant::now() + Duration::from_secs(1);
    let (mut tx, worker) = start(&db, true, opts);
    tx.run_script(
        "?[id, value] <- [[2, 20]] :put items {id => value}",
        BTreeMap::new(),
    )
    .unwrap();
    tx.commit().unwrap();
    worker.join().unwrap();
    assert!(
        callback.recv().is_err(),
        "stalled subscription must disconnect for reconciliation"
    );
    assert_eq!(run(&db, "?[id] := *items{id}, id == 2").rows.len(), 1);
    assert!(run(&db, "::warnings")
        .rows
        .iter()
        .any(|row| row[1].get_str() == Some("callback.delivery_timeout")));
}

#[test]
fn whole_deadline_expires_during_idle_before_longer_idle_limit() {
    let (_dir, db) = fixture("mem");
    let opts = GovernedTransactionOptions::new(Instant::now() + Duration::from_secs(1));
    let (mut tx, worker) = start(&db, false, opts);
    tx.run_script("?[x] <- [[1]]", BTreeMap::new()).unwrap();
    worker.join().unwrap();
    assert_eq!(
        code(tx.run_script("?[x] <- [[2]]", BTreeMap::new()).unwrap_err()),
        "eval::timeout"
    );
}

#[test]
fn explicit_kill_aborts_prior_writes_even_when_custom_rule_returns_late() {
    let (_dir, db) = fixture("mem");
    let (entered_tx, entered_rx) = bounded(1);
    let (release_tx, release_rx) = bounded(1);
    db.register_fixed_rule(
        "Hold".into(),
        SimpleFixedRule::new(1, move |_, _| {
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(NamedRows::new(
                vec!["x".into()],
                vec![vec![DataValue::from(1)]],
            ))
        }),
    )
    .unwrap();
    let (mut tx, worker) = start(&db, true, options());
    tx.run_script(
        "?[id, value] <- [[2, 20]] :put items {id => value}",
        BTreeMap::new(),
    )
    .unwrap();
    let caller = thread::spawn(move || {
        let result = tx.run_script("?[x] <~ Hold()", BTreeMap::new());
        assert!(tx.commit().is_err());
        result
    });
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let running = run(&db, "::running");
    assert_eq!(running.rows.len(), 1);
    let id = running.rows[0][0].get_int().unwrap();
    run(&db, &format!("::kill {id}"));
    release_tx.send(()).unwrap();
    assert_eq!(code(caller.join().unwrap().unwrap_err()), "eval::killed");
    worker.join().unwrap();
    assert!(run(&db, "::running").rows.is_empty());
    assert_eq!(
        run(&db, "?[id, value] := *items{id, value}").into_json()["rows"],
        json!([[1, 10]])
    );
}
