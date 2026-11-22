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

#[test]
fn hnsw_shadow_maintenance_handles_concurrent_puts_retractions_evictions_and_buffered_tx() {
    // Exercise every Shadow (WRITE_ONLY) dual-write path on an HNSW create when
    // the base relation has NO pre-existing search index (`has_search_indexes == false`):
    // 1. Backfill touches `a` and `b` (`pending_old` contains both).
    // 2. While paused at `shadow_active`:
    //    - `:put` updates `a` (IN `pending_old`, claimed by writer) and inserts `d` (NOT in `pending_old`)
    //    - `:rm` retracts `b` (IN `pending_old`, claimed by writer)
    //    - `:put` inserts then retracts `e` (NOT in `pending_old`)
    //    - `::evict` evicts `c` (pre-existing untouched key)
    //    - buffered `multi_transaction` commits `f`
    // 3. Builder drains remaining `pending_old` keys (`p` untouched during shadow) and publishes.
    let dir = tempfile::tempdir().unwrap();
    let db = DbInstance::new("rocksdb", dir.path().join("db").to_str().unwrap(), "{}").unwrap();
    run(
        &db,
        ":create doc {id:String,tt:TxTime => body:String,v:<F32;2>}",
    );
    run(
        &db,
        "?[id,body,v] <- [['a','a0',[0,0]],['b','b0',[1,0]],['c','c0',[2,0]],['p','p0',[3,0]]] :put doc{id=>body,v}",
    );

    // Stage 1: pause at `registered` and mutate `a`, `b`, `p` so `pending_old` is non-empty.
    let (reg_arrived, reg_release) = fence(&db, "registered");
    let building = start(&db, HNSW);
    arrive(&reg_arrived);
    finish(start(
        &db,
        "?[id,body,v] <- [['a','a1',[10,0]],['b','b1',[11,0]],['p','p1',[12,0]]] :put doc{id=>body,v}",
    ))
    .unwrap();

    // Re-arm fence for `shadow_active` before releasing `registered`.
    let (sh_arrived, sh_release) = fence(&db, "shadow_active");
    reg_release.send(()).unwrap();
    arrive(&sh_arrived);

    // Stage 2: live concurrent writes during active WRITE_ONLY shadow mode.
    finish(start(
        &db,
        "?[id,body,v] <- [['a','a2',[20,0]],['d','d0',[21,0]]] :put doc{id=>body,v}",
    ))
    .unwrap();
    finish(start(&db, "?[id] <- [['b']] :rm doc{id}")).unwrap();
    finish(start(
        &db,
        "?[id,body,v] <- [['e','e0',[22,0]]] :put doc{id=>body,v}",
    ))
    .unwrap();
    finish(start(&db, "?[id] <- [['e']] :rm doc{id}")).unwrap();
    finish(start(&db, "::evict doc [['c']]")).unwrap();

    let mtx = db.multi_transaction(true);
    mtx.run_script(
        "?[id,body,v] <- [['f','f0',[23,0]]] :put doc{id=>body,v}",
        BTreeMap::new(),
    )
    .unwrap();
    mtx.commit().unwrap();

    sh_release.send(()).unwrap();
    finish(building).unwrap();

    // Expected live set: a=[20,0], d=[21,0], f=[23,0], p=[12,0]. Gone: b, c, e.
    assert_eq!(
        rows(
            &db,
            "?[id] := ~doc:vec{id | query:vec([20,0]),k:50,ef:100} :order id"
        ),
        json!([["a"], ["d"], ["f"], ["p"]])
    );
    // Exact nearest match for each surviving key's latest vector.
    assert_eq!(
        rows(&db, "?[id] := ~doc:vec{id | query:vec([20,0]),k:1,ef:50}"),
        json!([["a"]])
    );
    assert_eq!(
        rows(&db, "?[id] := ~doc:vec{id | query:vec([12,0]),k:1,ef:50}"),
        json!([["p"]])
    );
    assert_eq!(
        rows(&db, "?[id] := ~doc:vec{id | query:vec([23,0]),k:1,ef:50}"),
        json!([["f"]])
    );
}

#[test]
fn hnsw_shadow_build_abort_preserves_committed_writes_and_allows_retry() {
    let dir = tempfile::tempdir().unwrap();
    let db = database(&dir);
    let (reg_arrived, reg_release) = fence(&db, "registered");
    let building = start(&db, HNSW);
    arrive(&reg_arrived);
    finish(start(
        &db,
        "?[id,body,v] <- [['a','a1',[4,4]]] :put doc{id=>body,v}",
    ))
    .unwrap();

    let (sh_arrived, sh_release) = fence(&db, "shadow_active");
    reg_release.send(()).unwrap();
    arrive(&sh_arrived);

    // Concurrent write updates shadow index while build is paused in shadow mode.
    finish(start(
        &db,
        "?[id,body,v] <- [['c','c0',[9,9]]] :put doc{id=>body,v}",
    ))
    .unwrap();

    // Fail the next commit (builder's shadow drain commit) and release.
    db.fail_next_commit_for_tests();
    sh_release.send(()).unwrap();
    assert!(finish(building)
        .unwrap_err()
        .contains("injected commit failure"));

    let DbInstance::RocksDb(inner) = &db else {
        unreachable!()
    };
    inner.set_tt_index_build_fence_for_tests(None);

    // Index was NOT published; committed base writes remain intact.
    assert!(db
        .run_script(
            "?[id] := ~doc:vec{id | query:vec([9,9]),k:5,ef:50}",
            BTreeMap::new(),
            ScriptMutability::Immutable
        )
        .is_err());
    assert_eq!(
        rows(&db, "?[id] := *doc{id} :order id"),
        json!([["a"], ["b"], ["c"]])
    );

    // Subsequent HNSW create succeeds cleanly over all committed rows.
    run(&db, HNSW);
    assert_eq!(
        rows(
            &db,
            "?[id] := ~doc:vec{id | query:vec([9,9]),k:10,ef:50} :order id"
        ),
        json!([["a"], ["b"], ["c"]])
    );
}

#[test]
fn hnsw_shadow_reindex_with_fts_keeps_exact_counts_stats_and_post_pub_writes() {
    let dir = tempfile::tempdir().unwrap();
    let db = database(&dir);
    db.enable_persisted_fts_stats().unwrap();
    run(&db, FTS);
    run(&db, HNSW);
    assert_eq!(db.seed_fts_stats().unwrap(), 1);

    let (reg_arrived, reg_release) = fence(&db, "registered");
    let reindexing = start(&db, "::reindex doc");
    arrive(&reg_arrived);
    finish(start(
        &db,
        "?[id,body,v] <- [['a','shadow one',[3,3]],['c','shadow two',[4,4]]] :put doc{id=>body,v}",
    ))
    .unwrap();

    let (sh_arrived, sh_release) = fence(&db, "shadow_active");
    reg_release.send(()).unwrap();
    arrive(&sh_arrived);

    // Multi-row concurrent writes while BOTH FTS and HNSW rebuild in shadow mode.
    finish(start(
        &db,
        "?[id,body,v] <- [['a','shadow updated',[5,5]],['d','shadow extra',[6,6]]] :put doc{id=>body,v}",
    ))
    .unwrap();
    finish(start(&db, "?[id] <- [['b']] :rm doc{id}")).unwrap();

    sh_release.send(()).unwrap();
    finish(reindexing).unwrap();

    // Immediate post-publication write must not double-maintain or fail.
    run(
        &db,
        "?[id,body,v] <- [['e','post pub',[7,7]]] :put doc{id=>body,v}",
    );

    // Persisted FTS stats reset to unseeded on reindex, then reseed cleanly.
    assert!(db
        .verify_fts_stats()
        .unwrap()
        .iter()
        .all(|c| c.persisted.is_none()));
    assert_eq!(db.seed_fts_stats().unwrap(), 1);
    for check in db.verify_fts_stats().unwrap() {
        assert_eq!(check.persisted, Some(check.scanned), "{}", check.index);
    }

    assert_eq!(
        rows(
            &db,
            "?[id] := ~doc:text{id | query:'shadow OR post',k:50} :order id"
        ),
        json!([["a"], ["c"], ["d"], ["e"]])
    );
    assert_eq!(
        rows(
            &db,
            "?[id] := ~doc:vec{id | query:vec([7,7]),k:50,ef:100} :order id"
        ),
        json!([["a"], ["c"], ["d"], ["e"]])
    );
}

#[test]
fn hnsw_extend_candidates_concurrent_replacements_stay_consistent() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        DbInstance::new("rocksdb", dir.path().join("db").to_str().unwrap(), "{}").unwrap(),
    );
    run(&db, ":create doc {id: String, tt: TxTime => v: <F32; 4>}");
    for i in 0..24 {
        let a = (i as f32) * 0.1 + 1.0;
        let b = ((i % 7) as f32) * 0.2 + 0.5;
        run(
            &db,
            &format!("?[id,v] <- [['k{i}', [{a}, {b}, 0.3, 0.7]]] :put doc{{id=>v}}"),
        );
    }
    run(
        &db,
        "::hnsw create doc:vec {dim: 4, m: 4, dtype: F32, fields: [v], distance: Cosine, ef_construction: 32, extend_candidates: true}",
    );
    let handles: Vec<_> = (0..4)
        .map(|w| {
            let db = db.clone();
            std::thread::spawn(move || {
                for round in 0..8 {
                    let i = (w * 6 + round) % 24;
                    let a = (round as f32) * 0.3 + (w as f32) + 0.5;
                    let b = (i as f32) * 0.05 + 0.2;
                    run(
                        &db,
                        &format!(
                            "?[id,v] <- [['k{i}', [{a}, {b}, 0.4, 0.9]]] :put doc{{id=>v}}"
                        ),
                    );
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let count = rows(&db, "?[count(id)] := *doc{id}");
    assert_eq!(count, json!([[24]]));
    let found = rows(
        &db,
        "?[id] := ~doc:vec{id | query: vec([1.0, 0.5, 0.4, 0.9]), k: 24, ef: 64} :order id",
    );
    assert_eq!(found.as_array().unwrap().len(), 24);
}

#[test]
fn hnsw_shadow_persistent_cache_drain_handles_large_backlog_and_concurrent_writes() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        DbInstance::new("rocksdb", dir.path().join("db").to_str().unwrap(), "{}").unwrap(),
    );
    run(&db, ":create doc {id: String, tt: TxTime => v: <F32; 2>}");
    for i in 0..64 {
        let x = (i as f32) + 1.0;
        let y = ((i % 9) as f32) + 0.5;
        run(
            &db,
            &format!("?[id,v] <- [['d{i:03}', [{x}, {y}]]] :put doc{{id=>v}}"),
        );
    }

    let (reg_arrived, reg_release) = fence(&db, "registered");
    let building = start(
        &db,
        "::hnsw create doc:vec {dim: 2, m: 8, dtype: F32, fields: [v], distance: L2, ef_construction: 32}",
    );
    arrive(&reg_arrived);

    // Mutate > 32 distinct keys while paused at `registered` so Phase A unlocked
    // catch-up must execute before Phase B `shadow_active` activates.
    for i in 0..40 {
        let x = (i as f32) + 100.0;
        run(
            &db,
            &format!("?[id,v] <- [['d{i:03}', [{x}, 2.0]]] :put doc{{id=>v}}"),
        );
    }
    for i in 40..45 {
        run(&db, &format!("?[id] <- [['d{i:03}']] :rm doc{{id}}"));
    }
    for i in 64..70 {
        let x = (i as f32) + 200.0;
        run(
            &db,
            &format!("?[id,v] <- [['d{i:03}', [{x}, 3.0]]] :put doc{{id=>v}}"),
        );
    }

    let (sh_arrived, sh_release) = fence(&db, "shadow_active");
    reg_release.send(()).unwrap();
    arrive(&sh_arrived);

    // Concurrent updates/inserts/retracts while shadow mode is active.
    finish(start(
        &db,
        "?[id,v] <- [['d000', [500.0, 5.0]], ['d070', [501.0, 6.0]]] :put doc{id=>v}",
    ))
    .unwrap();
    finish(start(&db, "?[id] <- [['d001']] :rm doc{id}")).unwrap();

    sh_release.send(()).unwrap();
    finish(building).unwrap();

    // Expected live rows: 64 - 5 (d040..d044) + 6 (d064..d069) + 1 (d070) - 1 (d001) = 65.
    assert_eq!(rows(&db, "?[count(id)] := *doc{id}"), json!([[65]]));
    let found = rows(
        &db,
        "?[id] := ~doc:vec{id | query: vec([500.0, 5.0]), k: 100, ef: 128} :order id",
    );
    let expected = rows(&db, "?[id] := *doc{id} :order id");
    assert_eq!(found, expected);
}

#[test]
fn fts_sst_backfill_concurrent_writes_and_persisted_stats_match() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        DbInstance::new("rocksdb", dir.path().join("db").to_str().unwrap(), "{}").unwrap(),
    );
    db.enable_persisted_fts_stats().unwrap();
    run(&db, ":create doc {id: String, tt: TxTime => body: String}");
    for i in 0..120 {
        run(
            &db,
            &format!(
                "?[id,body] <- [['d{i:03}', 'alpha common term{i}']] :put doc{{id=>body}}"
            ),
        );
    }

    let (reg_arrived, reg_release) = fence(&db, "registered");
    let building = start(
        &db,
        "::fts create doc:text {extractor: body, tokenizer: Simple, filters: [Lowercase]}",
    );
    arrive(&reg_arrived);

    // Mutate during backfill (updates, retractions, and inserts).
    for i in 0..20 {
        run(
            &db,
            &format!(
                "?[id,body] <- [['d{i:03}', 'beta updated term{i}']] :put doc{{id=>body}}"
            ),
        );
    }
    for i in 20..30 {
        run(&db, &format!("?[id] <- [['d{i:03}']] :rm doc{{id}}"));
    }
    for i in 120..130 {
        run(
            &db,
            &format!(
                "?[id,body] <- [['d{i:03}', 'gamma inserted term{i}']] :put doc{{id=>body}}"
            ),
        );
    }

    reg_release.send(()).unwrap();
    finish(building).unwrap();

    // Verify exact FTS matches across backfill-only, updated, and inserted cohorts.
    let alpha = rows(
        &db,
        "?[id] := ~doc:text{id | query: 'alpha', k: 200} :order id",
    );
    assert_eq!(alpha.as_array().unwrap().len(), 90); // d030..d119
    let beta = rows(
        &db,
        "?[id] := ~doc:text{id | query: 'beta', k: 200} :order id",
    );
    assert_eq!(beta.as_array().unwrap().len(), 20); // d000..d019
    let gamma = rows(
        &db,
        "?[id] := ~doc:text{id | query: 'gamma', k: 200} :order id",
    );
    assert_eq!(gamma.as_array().unwrap().len(), 10); // d120..d129

    // Persisted stats seed and verify cleanly against full posting scan.
    assert_eq!(db.seed_fts_stats().unwrap(), 1);
    for check in db.verify_fts_stats().unwrap() {
        assert_eq!(check.persisted, Some(check.scanned), "{}", check.index);
        assert_eq!(check.scanned.1, 120, "exact live doc count");
    }
}
