// SPDX-License-Identifier: MPL-2.0
//! Persisted BM25 corpus totals (opt-in, RocksDB). See `src/fts/persist.rs`.
#![cfg(feature = "storage-rocksdb")]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use cozo::{DbInstance, FtsStatsOpen, NamedRows, ScriptMutability, FTS_DOC_STATS_SCANS};

const SEARCH: &str =
    "?[id, score] := ~doc:idx{id | query: 'alpha', k: 50, bind_score: score} :order id";

fn open(path: &Path) -> DbInstance {
    DbInstance::new("rocksdb", path.to_str().unwrap(), "").unwrap()
}

fn write(db: &DbInstance, script: &str) -> NamedRows {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("{script}: {e:?}"))
}

fn read(db: &DbInstance, script: &str) -> NamedRows {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Immutable)
        .unwrap_or_else(|e| panic!("{script}: {e:?}"))
}

fn scans() -> u64 {
    FTS_DOC_STATS_SCANS.with(|c| c.get())
}

fn setup(db: &DbInstance, docs: usize) {
    write(db, ":create doc {id: String => body: String}");
    write(
        db,
        "::fts create doc:idx {extractor: body, tokenizer: Simple, filters: [Lowercase]}",
    );
    let rows = (0..docs)
        .map(|i| format!("['d{i}', 'alpha {}']", vec!["pad"; i % 7].join(" ")))
        .collect::<Vec<_>>()
        .join(", ");
    write(
        db,
        &format!("?[id, body] <- [{rows}] :put doc {{id => body}}"),
    );
}

/// Every index's persisted totals equal a full scan in the same snapshot.
fn assert_exact(db: &DbInstance) {
    for check in db.verify_fts_stats().unwrap() {
        assert_eq!(
            check.persisted,
            Some(check.scanned),
            "index {} persisted totals differ from a scan",
            check.index
        );
    }
}

fn assert_unseeded(db: &DbInstance) {
    for check in db.verify_fts_stats().unwrap() {
        assert_eq!(
            check.persisted, None,
            "index {} should be unseeded",
            check.index
        );
    }
}

#[test]
fn seeded_totals_stay_exact_under_concurrent_writers_and_searches_never_scan() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir.path().join("db"));
    assert_eq!(
        db.enable_persisted_fts_stats().unwrap(),
        FtsStatsOpen::Reset
    );
    setup(&db, 200);
    assert_unseeded(&db);
    assert_eq!(db.seed_fts_stats().unwrap(), 1);
    assert_exact(&db);

    let stop = Arc::new(AtomicBool::new(false));
    let writers: Vec<_> = (0..3)
        .map(|w| {
            let (db, stop) = (db.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut i = 0;
                while !stop.load(Ordering::Relaxed) {
                    // Insert, update in place, update to the same text, delete.
                    let id = format!("w{w}-{}", i % 40);
                    let script = match i % 4 {
                        0 => format!("?[id, body] <- [['{id}', 'alpha beta']] :put doc {{id => body}}"),
                        1 => format!("?[id, body] <- [['{id}', 'alpha gamma gamma']] :put doc {{id => body}}"),
                        2 => format!("?[id, body] <- [['{id}', 'alpha gamma gamma']] :put doc {{id => body}}"),
                        _ => format!("?[id] <- [['{id}']] :rm doc {{id}}"),
                    };
                    // Conflicting writers are expected; retry on error.
                    let _ = db.run_script(&script, BTreeMap::new(), ScriptMutability::Mutable);
                    i += 1;
                }
            })
        })
        .collect();

    let before = scans();
    for _ in 0..200 {
        let rows = read(&db, SEARCH).rows;
        assert!(!rows.is_empty());
    }
    assert_eq!(scans(), before, "a search scanned postings for its totals");
    stop.store(true, Ordering::Relaxed);
    for w in writers {
        w.join().unwrap();
    }
    assert_exact(&db);
}

#[test]
fn scores_with_persisted_totals_equal_scores_without() {
    let dir = tempfile::tempdir().unwrap();
    let with = open(&dir.path().join("with"));
    let without = open(&dir.path().join("without"));
    with.enable_persisted_fts_stats().unwrap();
    for db in [&with, &without] {
        setup(db, 60);
        write(db, "?[id] <- [['d3'], ['d4']] :rm doc {id}");
    }
    with.seed_fts_stats().unwrap();
    for db in [&with, &without] {
        write(
            db,
            "?[id, body] <- [['n1', 'alpha alpha new']] :put doc {id => body}",
        );
    }
    assert_exact(&with);
    assert_eq!(read(&with, SEARCH).rows, read(&without, SEARCH).rows);
}

#[test]
fn a_write_transaction_sees_its_own_pending_change() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir.path().join("db"));
    db.enable_persisted_fts_stats().unwrap();
    setup(&db, 20);
    db.seed_fts_stats().unwrap();
    let reference = tempfile::tempdir().unwrap();
    let plain = open(&reference.path().join("db"));
    setup(&plain, 20);
    for d in [&db, &plain] {
        let tx = d.multi_transaction(true);
        tx.run_script(
            "?[id, body] <- [['x', 'alpha alpha alpha']] :put doc {id => body}",
            BTreeMap::new(),
        )
        .unwrap();
        let inside = tx.run_script(SEARCH, BTreeMap::new()).unwrap().rows;
        tx.abort().unwrap();
        assert!(inside.iter().any(|r| r[0].get_str() == Some("x")));
    }
    let a = {
        let tx = db.multi_transaction(true);
        tx.run_script(
            "?[id, body] <- [['x', 'alpha alpha alpha']] :put doc {id => body}",
            BTreeMap::new(),
        )
        .unwrap();
        let rows = tx.run_script(SEARCH, BTreeMap::new()).unwrap().rows;
        tx.abort().unwrap();
        rows
    };
    let b = {
        let tx = plain.multi_transaction(true);
        tx.run_script(
            "?[id, body] <- [['x', 'alpha alpha alpha']] :put doc {id => body}",
            BTreeMap::new(),
        )
        .unwrap();
        let rows = tx.run_script(SEARCH, BTreeMap::new()).unwrap().rows;
        tx.abort().unwrap();
        rows
    };
    assert_eq!(a, b);
    // The aborted transactions left no delta behind.
    assert_exact(&db);
}

#[test]
fn a_clean_close_keeps_the_records_and_the_first_search_does_not_scan() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    {
        let db = open(&path);
        db.enable_persisted_fts_stats().unwrap();
        setup(&db, 50);
        db.seed_fts_stats().unwrap();
        write(
            &db,
            "?[id, body] <- [['late', 'alpha']] :put doc {id => body}",
        );
    }
    let db = open(&path);
    assert_eq!(
        db.enable_persisted_fts_stats().unwrap(),
        FtsStatsOpen::Trusted
    );
    let before = scans();
    read(&db, SEARCH);
    assert_eq!(
        scans(),
        before,
        "the first search after a clean reopen scanned"
    );
    assert_exact(&db);
}

#[test]
fn any_write_without_maintenance_resets_the_records() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    {
        let db = open(&path);
        db.enable_persisted_fts_stats().unwrap();
        setup(&db, 30);
        db.seed_fts_stats().unwrap();
    }
    {
        // Persistence off: an older engine, or this engine with the option off.
        let db = open(&path);
        write(&db, "?[id] <- [['d1'], ['d2']] :rm doc {id}");
    }
    let db = open(&path);
    assert_eq!(
        db.enable_persisted_fts_stats().unwrap(),
        FtsStatsOpen::Reset
    );
    assert_unseeded(&db);
    db.seed_fts_stats().unwrap();
    assert_exact(&db);
}

#[test]
fn disabling_mid_run_resets_at_the_next_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    {
        let db = open(&path);
        db.enable_persisted_fts_stats().unwrap();
        setup(&db, 30);
        db.seed_fts_stats().unwrap();
        if let DbInstance::RocksDb(inner) = &db {
            inner.disable_persisted_fts_stats();
        }
        write(&db, "?[id] <- [['d5']] :rm doc {id}");
    }
    let db = open(&path);
    assert_eq!(
        db.enable_persisted_fts_stats().unwrap(),
        FtsStatsOpen::Reset
    );
}

#[test]
fn reindex_leaves_the_index_unseeded_and_reseeding_is_exact() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir.path().join("db"));
    db.enable_persisted_fts_stats().unwrap();
    setup(&db, 40);
    db.seed_fts_stats().unwrap();
    write(&db, "?[id, body] <- [['r', 'alpha']] :put doc {id => body}");
    write(&db, "::reindex doc");
    assert_unseeded(&db);
    write(
        &db,
        "?[id, body] <- [['s', 'alpha alpha']] :put doc {id => body}",
    );
    assert_eq!(db.seed_fts_stats().unwrap(), 1);
    assert_exact(&db);
}

#[test]
fn without_enabling_nothing_is_written_and_seed_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir.path().join("db"));
    setup(&db, 10);
    assert_eq!(db.seed_fts_stats().unwrap(), 0);
    assert_unseeded(&db);
}
