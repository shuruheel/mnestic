/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The change-feed's central contract: **subscribers see only what committed.**
//!
//! `register_callback`'s own doc says it delivers changes "when the requested
//! relation are successfully committed". Through 0.12.0 the multi-statement
//! transaction path (`run_multi_transaction`) broke that promise: it called
//! `send_callbacks` unconditionally *after* the commit, so a transaction whose
//! commit failed still published `Put`/`Rm` events for rows that were never
//! durable. Anything syncing off the feed — a search mirror, an audit log, a
//! cache — would silently diverge from the database. The single-statement path
//! (`execute_single`) always got this right; it commits with `?` before it
//! dispatches.
//!
//! Provoking a *genuine* commit failure needs an I/O error or a backend-specific
//! conflict, and there is no portable way to do that across `mem` / `sqlite` /
//! `rocksdb`. So these tests use the internal `test-hooks` switch
//! (`Db::fail_next_commit_for_tests`) to fail the commit before it is attempted,
//! which leaves exactly the state a real failure leaves: rolled back, nothing
//! durable. The branch under test — *did the commit succeed?* — is the real one.
//!
//! Run with: `cargo test -p mnestic --features test-hooks --test callback_commit_contract`

#![cfg(feature = "test-hooks")]

use std::collections::BTreeMap;
use std::time::Duration;

use cozo::{CallbackOp, DbInstance, NamedRows, ScriptMutability};

fn run(db: &DbInstance, script: &str) -> NamedRows {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("script failed: {e:?}\n--- script ---\n{script}"))
}

/// The mem backend: the change-feed dispatch under test lives in
/// `run_multi_transaction` and is backend-independent, so the cheapest store
/// that supports multi-transactions is the honest one to use here.
fn setup() -> DbInstance {
    let db = DbInstance::new("mem", "", Default::default()).unwrap();
    run(&db, ":create kv {k: Int => v: Int}");
    db
}

/// Drain whatever the feed has to offer, giving the dispatcher a moment to run.
///
/// A macro rather than a function because `register_callback` returns a
/// `crossbeam::channel::Receiver`, which the crate does not re-export — naming
/// the type from an integration test would mean declaring crossbeam as a
/// dev-dependency just to spell a parameter.
macro_rules! drain {
    ($rx:expr) => {{
        // The dispatcher runs on the committing thread, so anything owed to us
        // has already been sent by the time `commit()` returns — but give it a
        // beat anyway, so a false green cannot come from reading too early.
        std::thread::sleep(Duration::from_millis(50));
        let mut out: Vec<CallbackOp> = vec![];
        while let Ok((op, _, _)) = $rx.try_recv() {
            out.push(op);
        }
        out
    }};
}

/// **The bug.** A multi-transaction whose commit fails must publish nothing.
///
/// Discrimination: revert the `db.rs` fix (dispatch unconditionally) and this
/// goes red — the subscriber receives a `Put` for a row that was never durable.
#[test]
fn failed_multi_tx_commit_publishes_nothing() {
    let db = setup();
    let (_id, rx) = db.register_callback("kv", None);

    let tx = db.multi_transaction(true);
    tx.run_script("?[k, v] <- [[1, 10]] :put kv {k => v}", BTreeMap::new())
        .unwrap();

    db.fail_next_commit_for_tests();
    let commit = tx.commit();

    assert!(
        commit.is_err(),
        "the test hook must actually fail the commit — otherwise this test proves nothing"
    );
    assert_eq!(
        drain!(rx),
        Vec::<CallbackOp>::new(),
        "PHANTOM EVENT: the change feed published a mutation for a transaction \
         whose commit failed — subscribers now believe in a row the database \
         does not have"
    );

    // And the database really is unchanged, so the feed and the store agree.
    let rows = db
        .run_script(
            "?[k, v] := *kv{k, v}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .unwrap()
        .rows;
    assert!(rows.is_empty(), "the failed commit left data behind");
}

/// The control: the *same* transaction shape, committed successfully, must
/// publish. Without this, the test above could pass by never delivering
/// anything at all.
#[test]
fn successful_multi_tx_commit_publishes() {
    let db = setup();
    let (_id, rx) = db.register_callback("kv", None);

    let tx = db.multi_transaction(true);
    tx.run_script("?[k, v] <- [[1, 10]] :put kv {k => v}", BTreeMap::new())
        .unwrap();
    tx.commit().unwrap();

    assert_eq!(
        drain!(rx),
        vec![CallbackOp::Put],
        "a committed multi-transaction must publish its mutation"
    );
}

/// An explicitly aborted transaction publishes nothing either. This path was
/// always correct (the `Abort` arm breaks without dispatching) — pinned so the
/// fix's restructuring of the `Commit` arm cannot disturb it.
#[test]
fn aborted_multi_tx_publishes_nothing() {
    let db = setup();
    let (_id, rx) = db.register_callback("kv", None);

    let tx = db.multi_transaction(true);
    tx.run_script("?[k, v] <- [[1, 10]] :put kv {k => v}", BTreeMap::new())
        .unwrap();
    tx.abort().unwrap();

    assert_eq!(
        drain!(rx),
        Vec::<CallbackOp>::new(),
        "an aborted transaction must publish nothing"
    );
}

/// The failure switch is one-shot: it must not poison the `Db` for subsequent
/// transactions. (Also guards the test suite itself — a sticky switch would make
/// every later test in this file vacuously green.)
#[test]
fn commit_failure_switch_is_one_shot() {
    let db = setup();
    let (_id, rx) = db.register_callback("kv", None);

    let tx1 = db.multi_transaction(true);
    tx1.run_script("?[k, v] <- [[1, 10]] :put kv {k => v}", BTreeMap::new())
        .unwrap();
    db.fail_next_commit_for_tests();
    assert!(tx1.commit().is_err());
    assert_eq!(drain!(rx), Vec::<CallbackOp>::new());

    let tx2 = db.multi_transaction(true);
    tx2.run_script("?[k, v] <- [[2, 20]] :put kv {k => v}", BTreeMap::new())
        .unwrap();
    tx2.commit()
        .expect("the switch must have cleared itself after firing once");

    assert_eq!(
        drain!(rx),
        vec![CallbackOp::Put],
        "the transaction after a failed one must still publish"
    );
}
