/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Correctness guard for the plain-snapshot read path on RocksDB: read-only
//! scripts no longer open a pessimistic transaction, so pin the isolation
//! semantics they must keep — uncommitted writes invisible, committed writes
//! visible to subsequent reads, and reads proceeding while a writer holds an
//! open transaction.

#![cfg(feature = "storage-rocksdb")]

use cozo::{DbInstance, ScriptMutability};
use std::collections::BTreeMap;

#[test]
fn snapshot_reads_are_isolated_from_open_writers() {
    let dir = tempfile::tempdir().unwrap();
    let db = DbInstance::new("rocksdb", dir.path().to_str().unwrap(), "").unwrap();
    db.run_script(
        ":create kv { k: Int => v: Int }",
        BTreeMap::new(),
        ScriptMutability::Mutable,
    )
    .unwrap();
    db.run_script(
        "?[k, v] <- [[1, 10]] :put kv { k => v }",
        BTreeMap::new(),
        ScriptMutability::Mutable,
    )
    .unwrap();

    // Open a write transaction and mutate without committing.
    let tx = db.multi_transaction(true);
    tx.run_script(
        "?[k, v] <- [[1, 20], [2, 200]] :put kv { k => v }",
        BTreeMap::new(),
    )
    .unwrap();

    // A read-only script (snapshot path) must see the pre-write state and
    // must not block on the writer's open locks.
    let res = db
        .run_script(
            "?[k, v] := *kv{k, v} :order k",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .unwrap();
    assert_eq!(
        res.rows.len(),
        1,
        "uncommitted write leaked into snapshot read"
    );
    assert_eq!(res.rows[0][1].get_int().unwrap(), 10);

    tx.commit().unwrap();

    // After commit, a fresh read-only script sees the new state.
    let res = db
        .run_script(
            "?[k, v] := *kv{k, v} :order k",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .unwrap();
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0][1].get_int().unwrap(), 20);
    assert_eq!(res.rows[1][1].get_int().unwrap(), 200);
}

#[test]
fn snapshot_view_is_stable_within_a_read_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let db = DbInstance::new("rocksdb", dir.path().to_str().unwrap(), "").unwrap();
    db.run_script(
        ":create kv { k: Int => v: Int }",
        BTreeMap::new(),
        ScriptMutability::Mutable,
    )
    .unwrap();
    db.run_script(
        "?[k, v] <- [[1, 1]] :put kv { k => v }",
        BTreeMap::new(),
        ScriptMutability::Mutable,
    )
    .unwrap();

    // Hold a read-only multi-statement transaction across a concurrent commit:
    // its view must stay pinned to its snapshot.
    let read_tx = db.multi_transaction(false);
    let before = read_tx
        .run_script("?[v] := *kv{k: 1, v}", BTreeMap::new())
        .unwrap();
    assert_eq!(before.rows[0][0].get_int().unwrap(), 1);

    db.run_script(
        "?[k, v] <- [[1, 2]] :put kv { k => v }",
        BTreeMap::new(),
        ScriptMutability::Mutable,
    )
    .unwrap();

    let during = read_tx
        .run_script("?[v] := *kv{k: 1, v}", BTreeMap::new())
        .unwrap();
    assert_eq!(
        during.rows[0][0].get_int().unwrap(),
        1,
        "read transaction's snapshot view drifted after a concurrent commit"
    );
    drop(read_tx);

    let after = db
        .run_script(
            "?[v] := *kv{k: 1, v}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .unwrap();
    assert_eq!(after.rows[0][0].get_int().unwrap(), 2);
}
