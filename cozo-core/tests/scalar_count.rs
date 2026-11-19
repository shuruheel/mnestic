/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 * This Source Code Form is subject to the terms of the Mozilla Public License,
 * v. 2.0. If a copy was not distributed with this file, obtain one at
 * https://mozilla.org/MPL/2.0/.
 */

use cozo::{DbInstance, GovernedTransactionOptions, ScriptMutability};
use std::collections::BTreeMap;
use std::thread;
use std::time::{Duration, Instant};

const COUNT: &str = "?[count(id)] := *vectors{id}";
const ENUMERATE: &str = "?[sum(one)] := *vectors{id}, one = 1";

fn run(db: &DbInstance, query: &str) -> Vec<Vec<cozo::DataValue>> {
    db.run_script(query, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap()
        .rows
}

fn fixture(engine: &str) -> (tempfile::TempDir, DbInstance) {
    let dir = tempfile::tempdir().unwrap();
    let db = DbInstance::new(engine, dir.path().join("db").to_str().unwrap(), "").unwrap();
    run(&db, ":create vectors { id: Int => embedding: <F32; 3> }");
    (dir, db)
}

fn contract(engine: &str) {
    let (_dir, db) = fixture(engine);
    assert_eq!(run(&db, COUNT), run(&db, ENUMERATE));
    for n in [1, 17, 257] {
        run(&db, &format!("?[id, embedding] := id in range(0, {n}), embedding = vec([1.0, 0.0, -1.0]) :put vectors {{id => embedding}}"));
        assert_eq!(run(&db, COUNT), vec![vec![cozo::DataValue::from(n)]]);
        assert_eq!(run(&db, COUNT), run(&db, ENUMERATE));
        // Projection multiplicity: every equal embedding is still counted.
        assert_eq!(
            run(&db, "?[count(embedding)] := *vectors{embedding}"),
            run(&db, COUNT)
        );
    }
    run(&db, "?[id] <- [[0], [1]] :rm vectors {id}");
    assert_eq!(run(&db, COUNT), vec![vec![cozo::DataValue::from(255)]]);
    assert_eq!(run(&db, COUNT), run(&db, ENUMERATE));
    // Conditions and joined multiplicity must retain the ordinary evaluator.
    assert_eq!(
        run(&db, "?[count(id)] := *vectors{id}, id < 5"),
        vec![vec![cozo::DataValue::from(3)]]
    );
    assert_eq!(
        run(&db, "?[count(id)] := *vectors{id}, other in [1, 2]"),
        vec![vec![cozo::DataValue::from(510)]]
    );
    assert_eq!(
        run(&db, "?[count_unique(embedding)] := *vectors{embedding}"),
        vec![vec![cozo::DataValue::from(1)]]
    );
    assert_eq!(
        run(&db, "?[count(id)] := *vectors{id} :limit 0"),
        Vec::<Vec<cozo::DataValue>>::new()
    );
    assert!(db
        .run_script(
            "?[count(id)] := *missing{id}",
            BTreeMap::new(),
            ScriptMutability::Immutable
        )
        .is_err());
    // A tiny request deadline still terminates a count through the usual error path.
    let err = db
        .run_script(
            &format!("{COUNT} :timeout 0.000000001"),
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .unwrap_err();
    assert_eq!(err.code().unwrap().to_string(), "eval::timeout");
}

#[test]
fn scalar_count_mem_contract() {
    contract("mem");
}
#[cfg(feature = "storage-sqlite")]
#[test]
fn scalar_count_sqlite_contract() {
    contract("sqlite");
}
#[cfg(feature = "storage-rocksdb")]
#[test]
fn scalar_count_rocks_contract() {
    contract("rocksdb");
}

fn transaction_contract(engine: &str) {
    let (_dir, db) = fixture(engine);
    let (mut tx, worker) = db
        .governed_transaction(
            true,
            GovernedTransactionOptions::new(Instant::now() + Duration::from_secs(10)),
        )
        .unwrap();
    let worker = thread::spawn(move || worker.run());
    tx.run_script(
        "?[id, embedding] <- [[1, vec([1, 0, 0])]] :put vectors {id => embedding}",
        BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(
        tx.run_script(COUNT, BTreeMap::new()).unwrap().rows,
        vec![vec![cozo::DataValue::from(1)]]
    );
    tx.abort().unwrap();
    worker.join().unwrap();
    assert_eq!(run(&db, COUNT), vec![vec![cozo::DataValue::from(0)]]);
}
#[test]
fn scalar_count_mem_rollback() {
    transaction_contract("mem");
}
#[cfg(feature = "storage-sqlite")]
#[test]
fn scalar_count_sqlite_rollback() {
    transaction_contract("sqlite");
}
#[cfg(feature = "storage-rocksdb")]
#[test]
fn scalar_count_rocks_rollback() {
    transaction_contract("rocksdb");
}

#[cfg(feature = "storage-rocksdb")]
#[test]
fn scalar_count_rocks_pinned_snapshot_and_reopen() {
    let (dir, db) = fixture("rocksdb");
    run(
        &db,
        "?[id, embedding] <- [[1, vec([1, 0, 0])]] :put vectors {id => embedding}",
    );
    let (mut reader, worker) = db
        .governed_transaction(
            false,
            GovernedTransactionOptions::new(Instant::now() + Duration::from_secs(10)),
        )
        .unwrap();
    let worker = thread::spawn(move || worker.run());
    assert_eq!(
        reader.run_script(COUNT, BTreeMap::new()).unwrap().rows,
        vec![vec![cozo::DataValue::from(1)]]
    );
    run(
        &db,
        "?[id, embedding] <- [[2, vec([0, 1, 0])]] :put vectors {id => embedding}",
    );
    assert_eq!(
        reader.run_script(COUNT, BTreeMap::new()).unwrap().rows,
        vec![vec![cozo::DataValue::from(1)]]
    );
    reader.commit().unwrap();
    worker.join().unwrap();
    assert_eq!(run(&db, COUNT), vec![vec![cozo::DataValue::from(2)]]);
    drop(db);
    let db = DbInstance::new("rocksdb", dir.path().join("db").to_str().unwrap(), "").unwrap();
    assert_eq!(run(&db, COUNT), run(&db, ENUMERATE));
    assert_eq!(run(&db, COUNT), vec![vec![cozo::DataValue::from(2)]]);
}
