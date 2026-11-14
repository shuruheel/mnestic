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
        "::repair_corrupt doc",
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
fn eviction_uses_exact_captured_versions_before_queued_index_build() {
    let dir = tempfile::tempdir().unwrap();
    let db = database(&dir);
    run(&db, "::evict doc []"); // reserve audit metadata before the race
    let (arrived, release) = fence(&db, "evict_pre_commit");
    let eviction = start(&db, "::evict doc [['a']]");
    arrive(&arrived);
    finish(start(
        &db,
        "?[id,body,v] <- [['a','surviving alpha',[9,9]]] :put doc{id=>body,v}",
    ))
    .unwrap();
    // Eviction holds the native audit-salt lock, so maintenance must drain it.
    // A newer version committed before eviction still survives exact deletion.
    let building = start(&db, FTS);
    release.send(()).unwrap();
    finish(eviction).unwrap();
    finish(building).unwrap();
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

#[test]
fn buffered_writers_and_governed_transactions_follow_rebuilt_and_dropped_indexes() {
    for action in ["::reindex doc", "::index drop doc:text"] {
        let dir = tempfile::tempdir().unwrap();
        let db = database(&dir);
        for config in [FTS, HNSW, LSH] {
            run(&db, config);
        }
        let options = cozo::GovernedTransactionOptions::new(
            std::time::Instant::now() + Duration::from_secs(20),
        );
        let (mut governed, worker) = db.governed_transaction(true, options).unwrap();
        let worker = std::thread::spawn(move || worker.run());
        governed
            .run_script(
                "?[id,body,v] <- [['a','governed alpha',[8,8]]] :put doc{id=>body,v}",
                BTreeMap::new(),
            )
            .unwrap();
        let legacy = db.multi_transaction(true);
        legacy.run_script("?[id,body,v] <- [['a','legacy alpha',[9,9]],['c','new gamma',[7,7]]] :put doc{id=>body,v}", BTreeMap::new()).unwrap();
        finish(start(&db, action)).unwrap();
        governed.commit().unwrap();
        worker.join().unwrap();
        legacy.commit().unwrap();
        assert_eq!(
            rows(&db, "?[id] := ~doc:vec{id | query:vec([9,9]),k:1,ef:50}"),
            json!([["a"]])
        );
        assert_eq!(
            rows(&db, "?[body] := *doc{id:'a',body}"),
            json!([["legacy alpha"]])
        );
        if action.contains("reindex") {
            assert_eq!(
                rows(&db, "?[id] := ~doc:text{id | query:'legacy',k:10}"),
                json!([["a"]])
            );
            assert_eq!(
                rows(&db, "?[id] := ~doc:text{id | query:'governed',k:10}"),
                json!([])
            );
        } else {
            run(&db, FTS);
            assert_eq!(
                rows(&db, "?[id] := ~doc:text{id | query:'legacy',k:10}"),
                json!([["a"]])
            );
        }
    }
}

#[test]
fn failed_build_commits_leave_no_partial_name_and_rebuilds_keep_old_generation() {
    for stage in ["registered", "populated"] {
        for rebuild in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let db = database(&dir);
            if rebuild {
                run(&db, FTS);
            }
            let (arrived, release) = fence(&db, stage);
            let building = start(&db, if rebuild { "::reindex doc" } else { FTS });
            arrive(&arrived);
            db.fail_next_commit_for_tests();
            release.send(()).unwrap();
            assert!(finish(building)
                .unwrap_err()
                .contains("injected commit failure"));
            let DbInstance::RocksDb(inner) = &db else {
                unreachable!()
            };
            inner.set_tt_index_build_fence_for_tests(None);
            if rebuild {
                assert_eq!(
                    rows(&db, "?[id] := ~doc:text{id | query:'old',k:10} :order id"),
                    json!([["a"], ["b"]])
                );
            } else {
                assert!(db
                    .run_script(
                        "?[id] := ~doc:text{id | query:'old',k:10}",
                        BTreeMap::new(),
                        ScriptMutability::Immutable
                    )
                    .is_err());
            }
            run(&db, if rebuild { "::reindex doc" } else { FTS });
            run(
                &db,
                "?[id,body,v] <- [['a','fresh alpha',[4,4]]] :put doc{id=>body,v}",
            );
            assert_eq!(
                rows(&db, "?[id] := ~doc:text{id | query:'fresh',k:10}"),
                json!([["a"]])
            );
        }
    }
}

#[test]
fn deadline_after_backfill_does_not_attach_a_partial_index() {
    let dir = tempfile::tempdir().unwrap();
    let db = database(&dir);
    let (arrived, release) = fence(&db, "populated");
    let other = db.clone();
    let (send, done) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = other
            .run_script_with_options(
                FTS,
                BTreeMap::new(),
                ScriptMutability::Mutable,
                cozo::ScriptRunOptions::new().with_timeout(0.5),
            )
            .map(|_| ())
            .map_err(|e| format!("{e:?}"));
        send.send(result).unwrap();
    });
    arrive(&arrived);
    std::thread::sleep(Duration::from_millis(550));
    release.send(()).unwrap();
    assert!(finish(done).unwrap_err().contains("eval::timeout"));
    let DbInstance::RocksDb(inner) = &db else {
        unreachable!()
    };
    inner.set_tt_index_build_fence_for_tests(None);
    assert!(db
        .run_script(
            "?[id] := ~doc:text{id | query:'old',k:10}",
            BTreeMap::new(),
            ScriptMutability::Immutable
        )
        .is_err());
    run(&db, FTS);
}

#[test]
fn maintenance_drains_catalog_writers_then_releases_global_admission() {
    let dir = tempfile::tempdir().unwrap();
    let db = database(&dir);
    run(&db, FTS);
    run(&db, ":create other {id:String,tt:TxTime}");
    let (held, holding) = crossbeam::channel::bounded(1);
    let (release_writer, writer_release) = crossbeam::channel::bounded(1);
    db.register_fixed_rule(
        "Hold".into(),
        cozo::SimpleFixedRule::new(1, move |_, _| {
            held.send(()).unwrap();
            writer_release
                .recv_timeout(Duration::from_secs(20))
                .unwrap();
            Ok(NamedRows::new(vec!["x".into()], vec![vec![1.into()]]))
        }),
    )
    .unwrap();
    let writer = start(&db, "{ ::describe doc:text 'held metadata' } { ?[x] <~ Hold() } { ?[id] <- [['committed']] :put other{id} }");
    holding.recv_timeout(Duration::from_secs(20)).unwrap();
    let (arrived, release) = fence(&db, "publication_before_catalog");
    let building = start(&db, "::reindex doc");
    // Maintenance must first drain the admitted transaction. The holder can
    // finish its next statement and commit without waiting behind the build.
    release_writer.send(()).unwrap();
    finish(writer).unwrap();
    arrive(&arrived);
    // Once the target relation is exclusive, global admission is released:
    // an unrelated writer can still commit while publication is paused.
    finish(start(
        &db,
        "?[id] <- [['during publication']] :put other{id}",
    ))
    .unwrap();
    release.send(()).unwrap();
    finish(building).unwrap();
    assert_eq!(
        rows(&db, "?[id] := *other{id}"),
        json!([["committed"], ["during publication"]])
    );
}

#[test]
fn persisted_fts_totals_follow_tt_commits_eviction_rebuild_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let assert_exact = |db: &DbInstance| {
        let checks = db.verify_fts_stats().unwrap();
        assert_eq!(checks.len(), 1);
        for check in checks {
            assert_eq!(check.persisted, Some(check.scanned), "{}", check.index);
        }
    };
    {
        let db = database(&dir);
        db.enable_persisted_fts_stats().unwrap();
        run(&db, FTS);
        assert_eq!(db.seed_fts_stats().unwrap(), 1);
        assert_exact(&db);
        for n in 0..5 {
            run(
                &db,
                &format!(
                    "?[id,body,v] <- [['a','alpha {}',[0,0]]] :put doc{{id=>body,v}}",
                    "extra ".repeat(n)
                ),
            );
            assert_exact(&db);
        }
        let writers: Vec<_> = (0..4)
            .map(|n| {
                start(
                    &db,
                    &format!("?[id,body,v] <- [['w{n}','new alpha',[1,1]]] :put doc{{id=>body,v}}"),
                )
            })
            .collect();
        for writer in writers {
            finish(writer).unwrap();
        }
        assert_exact(&db);
        run(&db, "::evict doc [['a']]");
        assert_exact(&db);
        run(&db, "?[id] <- [['b']] :rm doc{id}");
        assert_exact(&db);
        run(&db, "::reindex doc");
        assert!(db
            .verify_fts_stats()
            .unwrap()
            .iter()
            .all(|c| c.persisted.is_none()));
        assert_eq!(db.seed_fts_stats().unwrap(), 1);
        assert_exact(&db);
    }
    let db = DbInstance::new("rocksdb", dir.path().join("db").to_str().unwrap(), "{}").unwrap();
    assert_eq!(
        db.enable_persisted_fts_stats().unwrap(),
        cozo::FtsStatsOpen::Trusted
    );
    assert_exact(&db);
    run(
        &db,
        "?[id,body,v] <- [['z','after reopen',[1,1]]] :put doc{id=>body,v}",
    );
    assert_exact(&db);
}

#[test]
fn mutation_deadlines_cover_admission_and_cancelled_waiters_do_not_block_publication() {
    let dir = tempfile::tempdir().unwrap();
    let db = database(&dir);
    let (arrived, release) = fence(&db, "publication_before_catalog");
    let building = start(&db, FTS);
    arrive(&arrived); // builder owns exclusive relation admission
    for script in [
        "?[id,body,v] <- [['a','cancelled alpha',[2,2]]] :put doc{id=>body,v}",
        "{?[id,body,v] <- [['a','cancelled alpha',[2,2]]] :put doc{id=>body,v}}",
        "::evict doc [['a']]",
    ] {
        let error = db
            .run_script_with_options(
                script,
                BTreeMap::new(),
                ScriptMutability::Mutable,
                cozo::ScriptRunOptions::new().with_timeout(0.02),
            )
            .unwrap_err();
        assert!(format!("{error:?}").contains("eval::timeout"));
    }
    release.send(()).unwrap();
    finish(building).unwrap();
    assert_eq!(
        rows(&db, "?[id] := ~doc:text{id | query:'old',k:10}"),
        json!([["a"], ["b"]])
    );
    run(
        &db,
        "?[id,body,v] <- [['a','after alpha',[3,3]]] :put doc{id=>body,v}",
    );
    assert_eq!(
        rows(&db, "?[id] := ~doc:text{id | query:'after',k:10}"),
        json!([["a"]])
    );
}
