/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file, You can obtain one at
 * https://mozilla.org/MPL/2.0/.
 */

#![cfg(all(feature = "storage-rocksdb", feature = "test-hooks"))]

use cozo::{DbInstance, NamedRows, ScriptMutability};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

fn run(db: &DbInstance, script: &str) -> NamedRows {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("{script}: {e:?}"))
}
fn rows(db: &DbInstance, script: &str) -> Value {
    run(db, script).into_json()["rows"].clone()
}
fn database(dir: &tempfile::TempDir) -> DbInstance {
    let db = DbInstance::new("rocksdb", dir.path().join("db").to_str().unwrap(), "{}").unwrap();
    run(
        &db,
        ":create doc {id:String,tt:TxTime => body:String,v:<F32;2>}",
    );
    run(
        &db,
        "?[id,body,v] <- [['a','old alpha',[0,0]],['b','old beta',[1,1]]] :put doc{id=>body,v}",
    );
    db
}
const FTS: &str = "::fts create doc:text {extractor:body,tokenizer:Simple,filters:[Lowercase]}";
const HNSW: &str =
    "::hnsw create doc:vec {fields:[v],dim:2,m:8,dtype:F32,distance:L2,ef_construction:50}";
const LSH: &str = "::lsh create doc:similar {extractor:body,tokenizer:Simple,filters:[Lowercase],n_perm:64,target_threshold:0.4,n_gram:1}";

fn fence(db: &DbInstance, stage: &'static str) -> (mpsc::Receiver<()>, mpsc::SyncSender<()>) {
    let (arrive, arrived) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let released = Mutex::new(released);
    let DbInstance::RocksDb(inner) = db else {
        unreachable!()
    };
    inner.set_tt_index_build_fence_for_tests(Some(Arc::new(move |at| {
        if at == stage {
            arrive.send(()).unwrap();
            released
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(30))
                .expect("fence release");
        }
    })));
    (arrived, release)
}
fn start(db: &DbInstance, script: &str) -> mpsc::Receiver<Result<(), String>> {
    let (send, recv) = mpsc::sync_channel(1);
    let db = db.clone();
    let script = script.to_owned();
    std::thread::spawn(move || {
        let result = db
            .run_script(&script, BTreeMap::new(), ScriptMutability::Mutable)
            .map(|_| ())
            .map_err(|e| format!("{e:?}"));
        let _ = send.send(result);
    });
    recv
}
fn finish(done: mpsc::Receiver<Result<(), String>>) -> Result<(), String> {
    done.recv_timeout(Duration::from_secs(30))
        .expect("operation completed without deadlock")
}
fn arrive(signal: &mpsc::Receiver<()>) {
    signal
        .recv_timeout(Duration::from_secs(30))
        .expect("build reached fence");
}

#[test]
fn every_kind_builds_and_rebuilds_without_blocking_commits() {
    for config in [FTS, HNSW, LSH] {
        for rebuild in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let db = database(&dir);
            if rebuild {
                run(&db, config);
            }
            let (arrived, release) = fence(&db, "registered");
            let building = start(&db, if rebuild { "::reindex doc" } else { config });
            arrive(&arrived);
            finish(start(&db,"?[id,body,v] <- [['a','new alpha',[4,4]],['c','new gamma',[5,5]]] :put doc{id=>body,v}")).unwrap();
            finish(start(&db, "::evict doc [['b']]")).unwrap();
            // GC may physically delete the old a record; the pinned snapshot
            // must still supply it during backfill and catch-up removal.
            let history = run(&db, "::history doc [['a']]");
            let col = history.headers.iter().position(|h| h == "tt").unwrap();
            let latest = history
                .rows
                .iter()
                .map(|r| r[col].get_int().unwrap())
                .max()
                .unwrap();
            finish(start(&db, &format!("::history_gc doc {latest}"))).unwrap();
            if rebuild && config == FTS {
                assert_eq!(
                    rows(&db, "?[id] := ~doc:text{id | query:'new',k:50} :order id"),
                    json!([["a"], ["c"]])
                );
            }
            release.send(()).unwrap();
            finish(building).unwrap();
            let query = if config == FTS {
                "?[id] := ~doc:text{id | query:'new',k:50} :order id"
            } else if config == HNSW {
                "?[id] := ~doc:vec{id | query:vec([0,0]),k:50,ef:100} :order id"
            } else {
                "?[id] := ~doc:similar{id | query:'new alpha',k:50} :order id"
            };
            let found = rows(&db, query);
            if config == LSH {
                assert!(found.as_array().unwrap().contains(&json!(["a"])));
                assert!(!found.as_array().unwrap().contains(&json!(["b"])));
            } else {
                assert_eq!(found, json!([["a"], ["c"]]));
            }
        }
    }
}

#[test]
fn buffered_transaction_resolves_indexes_attached_after_its_statement() {
    let dir = tempfile::tempdir().unwrap();
    let db = database(&dir);
    let tx = db.multi_transaction(true);
    tx.run_script(
        "?[id,body,v] <- [['a','pending alpha',[8,8]]] :put doc{id=>body,v}",
        BTreeMap::new(),
    )
    .unwrap();
    for config in [FTS, HNSW, LSH] {
        finish(start(&db, config)).unwrap();
    }
    tx.commit().unwrap();
    assert_eq!(
        rows(&db, "?[id] := ~doc:text{id | query:'pending',k:50}"),
        json!([["a"]])
    );
    assert_eq!(
        rows(&db, "?[id] := ~doc:vec{id | query:vec([8,8]),k:1,ef:50}"),
        json!([["a"]])
    );
}

#[test]
fn drop_rename_and_recreate_invalidate_an_unpublished_build() {
    for action in [
        "::index drop doc:text",
        "::rename doc -> renamed",
        "::remove doc",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let db = database(&dir);
        let (arrived, release) = fence(&db, "registered");
        let building = start(&db, FTS);
        arrive(&arrived);
        finish(start(&db, action)).unwrap();
        if action == "::remove doc" {
            run(
                &db,
                ":create doc {id:String,tt:TxTime => body:String,v:<F32;2>}",
            );
        }
        release.send(()).unwrap();
        assert!(finish(building).is_err());
        let base = if action.contains("rename") {
            "renamed"
        } else {
            "doc"
        };
        assert!(db
            .run_script(
                &format!("?[id] := ~{base}:text{{id | query:'old',k:10}}"),
                BTreeMap::new(),
                ScriptMutability::Immutable
            )
            .is_err());
    }
}

#[test]
fn catchup_overflow_fails_the_rebuild_and_keeps_the_attached_index() {
    let dir = tempfile::tempdir().unwrap();
    let db = database(&dir);
    run(&db, FTS);
    let DbInstance::RocksDb(inner) = &db else {
        unreachable!()
    };
    inner.set_tt_index_build_key_limit_for_tests(1);
    let (arrived, release) = fence(&db, "populated");
    let building = start(&db, "::reindex doc");
    arrive(&arrived);
    finish(start(
        &db,
        "?[id,body,v] <- [['a','new alpha',[3,3]],['c','new gamma',[4,4]]] :put doc{id=>body,v}",
    ))
    .unwrap();
    release.send(()).unwrap();
    assert!(finish(building).unwrap_err().contains("catch-up budget"));
    assert_eq!(
        rows(&db, "?[id] := ~doc:text{id | query:'new',k:50} :order id"),
        json!([["a"], ["c"]])
    );
}

#[test]
fn eviction_uses_exact_captured_versions_when_index_attaches_before_commit() {
    let dir = tempfile::tempdir().unwrap();
    let db = database(&dir);
    run(&db, "::evict doc []"); // reserve audit metadata before the race
    let (arrived, release) = fence(&db, "evict_pre_commit");
    let eviction = start(&db, "::evict doc [['a']]");
    arrive(&arrived);
    finish(start(&db, FTS)).unwrap();
    finish(start(
        &db,
        "?[id,body,v] <- [['a','surviving alpha',[9,9]]] :put doc{id=>body,v}",
    ))
    .unwrap();
    release.send(()).unwrap();
    finish(eviction).unwrap();
    assert_eq!(
        rows(&db, "?[id] := ~doc:text{id | query:'surviving',k:10}"),
        json!([["a"]])
    );
    assert_eq!(
        rows(&db, "?[body] := *doc{id:'a',body}"),
        json!([["surviving alpha"]])
    );
    assert_eq!(run(&db, "::history doc [['a']]").rows.len(), 1);
}
