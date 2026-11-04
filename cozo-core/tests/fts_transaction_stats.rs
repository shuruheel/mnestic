// SPDX-License-Identifier: MPL-2.0

use std::collections::BTreeMap;

use cozo::{DbInstance, MultiTransaction, NamedRows, ScriptMutability};

const QUERY: &str = "?[id,score] := ~doc:idx{id | query:'alpha',k:20,bind_score:score} :order id";
const TOP: &str = "?[id,score] := ~doc:idx{id | query:'alpha',k:1,bind_score:score}";

fn run(db: &DbInstance, script: &str) -> NamedRows {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("{script}: {e:?}"))
}

fn tx_run(tx: &MultiTransaction, script: &str) -> NamedRows {
    tx.run_script(script, BTreeMap::new()).unwrap()
}

fn setup(db: &DbInstance) {
    run(db, ":create doc {id:String => body:String}");
    run(
        db,
        "::fts create doc:idx {extractor:body,tokenizer:Simple,filters:[Lowercase]}",
    );
    run(db, &format!(
        "?[id,body] <- [['a','alpha alpha {}'],['b','alpha pad pad'],['c','{}']] :put doc {{id=>body}}",
        vec!["pad";8].join(" "), vec!["pad";100].join(" ")
    ));
}

fn backends(mut test: impl FnMut(&DbInstance)) {
    test(&DbInstance::new("mem", "", "").unwrap());
    #[cfg(feature = "storage-sqlite")]
    {
        let dir = tempfile::tempdir().unwrap();
        test(
            &DbInstance::new("sqlite", dir.path().join("db.sqlite").to_str().unwrap(), "").unwrap(),
        );
    }
    #[cfg(feature = "storage-rocksdb")]
    {
        let dir = tempfile::tempdir().unwrap();
        test(&DbInstance::new("rocksdb", dir.path().join("db").to_str().unwrap(), "").unwrap());
    }
}

#[test]
fn abort_and_handle_drop_preserve_complete_scores_and_top_one() {
    backends(|db| {
        setup(db);
        let before = run(db, QUERY).rows;
        let top = run(db, TOP).rows;
        assert_eq!(top[0][0].get_str(), Some("a"));
        for explicit_abort in [true, false] {
            let tx = db.multi_transaction(true);
            tx_run(&tx, "?[id] <- [['c']] :rm doc {id}");
            // Read-your-writes is allowed to change the score within the tx.
            assert_ne!(tx_run(&tx, QUERY).rows, before);
            if explicit_abort {
                tx.abort().unwrap();
            }
            drop(tx);
            assert_eq!(run(db, QUERY).rows, before);
            assert_eq!(run(db, TOP).rows, top);
        }
    });
}

#[test]
fn warm_serialized_reads_and_successful_writes_do_not_scan_the_corpus() {
    backends(|db| {
        setup(db);
        run(db, QUERY);
        cozo::FTS_DOC_STATS_SCANS.with(|n| n.set(0));
        for _ in 0..5 {
            run(db, QUERY);
        }
        assert_eq!(cozo::FTS_DOC_STATS_SCANS.with(|n| n.get()), 0);
        run(
            db,
            "?[id,body] <- [['b','alpha pad pad extra']] :put doc {id=>body}",
        );
        cozo::FTS_DOC_STATS_SCANS.with(|n| n.set(0));
        run(db, QUERY);
        assert_eq!(cozo::FTS_DOC_STATS_SCANS.with(|n| n.get()), 0);
    });
}

#[test]
fn committed_updates_rebuilds_and_recreated_index_match_fresh_postings() {
    backends(|db| {
        setup(db);
        let tx = db.multi_transaction(true);
        tx_run(
            &tx,
            "?[id,body] <- [['b','alpha alpha alpha pad'],['empty','']] :put doc {id=>body}",
        );
        let pending = tx_run(&tx, QUERY).rows;
        tx.commit().unwrap();
        drop(tx);
        assert_eq!(run(db, QUERY).rows, pending);
        run(db, "::reindex doc");
        assert_eq!(run(db, QUERY).rows, pending);
        run(db, "::index drop doc:idx");
        run(
            db,
            "::fts create doc:idx {extractor:body,tokenizer:Simple,filters:[Lowercase]}",
        );
        assert_eq!(run(db, QUERY).rows, pending);
    });
}

#[test]
fn failed_multistatement_script_does_not_change_committed_statistics() {
    backends(|db| {
        setup(db);
        let before = run(db, QUERY).rows;
        assert!(db
            .run_script(
                "{?[id]<-[['c']] :rm doc {id}} {?[v] := *missing_relation{v}}",
                BTreeMap::new(),
                ScriptMutability::Mutable,
            )
            .is_err());
        assert_eq!(run(db, QUERY).rows, before);
    });
}

#[test]
fn prefixes_keep_lowercase_normalization() {
    backends(|db| {
        setup(db);
        let prefix = run(db, "?[id] := ~doc:idx{id | query:'ALP*',k:20} :order id");
        let exact = run(db, "?[id] := ~doc:idx{id | query:'alpha',k:20} :order id");
        assert_eq!(prefix.rows, exact.rows);
    });
}

#[cfg(feature = "storage-rocksdb")]
#[test]
fn an_old_rocks_snapshot_cannot_reuse_or_publish_newer_statistics() {
    let dir = tempfile::tempdir().unwrap();
    let db = DbInstance::new("rocksdb", dir.path().join("db").to_str().unwrap(), "").unwrap();
    setup(&db);
    let before = run(&db, QUERY).rows;
    let old_warm = db.multi_transaction(false);
    assert_eq!(tx_run(&old_warm, QUERY).rows, before);
    let old_cold = db.multi_transaction(false);
    tx_run(&old_cold, "?[v] <- [[1]]");

    run(&db, "?[id] <- [['c']] :rm doc {id}");
    let after = run(&db, QUERY).rows;
    assert_ne!(after, before);
    assert_eq!(tx_run(&old_warm, QUERY).rows, before);
    // First FTS read of an old snapshot must scan its own posting population.
    assert_eq!(tx_run(&old_cold, QUERY).rows, before);
    assert_eq!(run(&db, QUERY).rows, after);
    old_warm.abort().unwrap();
    old_cold.abort().unwrap();
}

#[cfg(feature = "storage-rocksdb")]
#[test]
fn an_open_writer_does_not_change_another_readers_scores() {
    let dir = tempfile::tempdir().unwrap();
    let db = DbInstance::new("rocksdb", dir.path().join("db").to_str().unwrap(), "").unwrap();
    setup(&db);
    let before = run(&db, QUERY).rows;
    let writer = db.multi_transaction(true);
    tx_run(&writer, "?[id] <- [['c']] :rm doc {id}");
    let reader = db.multi_transaction(false);
    assert_eq!(tx_run(&reader, QUERY).rows, before);
    writer.commit().unwrap();
    assert_ne!(run(&db, QUERY).rows, before);
    assert_eq!(tx_run(&reader, QUERY).rows, before);
    reader.abort().unwrap();
}

#[cfg(feature = "storage-rocksdb")]
#[test]
fn overlapping_commits_match_a_rebuilt_combined_corpus() {
    for first_left in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let db = DbInstance::new("rocksdb", dir.path().join("db").to_str().unwrap(), "").unwrap();
        setup(&db);
        let left = db.multi_transaction(true);
        let right = db.multi_transaction(true);
        tx_run(
            &left,
            "?[id,body] <- [['left','alpha alpha pad pad']] :put doc {id=>body}",
        );
        tx_run(
            &right,
            "?[id,body] <- [['right','alpha']] :put doc {id=>body}",
        );
        if first_left {
            left.commit().unwrap();
            right.commit().unwrap();
        } else {
            right.commit().unwrap();
            left.commit().unwrap();
        }
        let combined = run(&db, QUERY).rows;
        assert_eq!(combined.len(), 4);
        run(&db, "::reindex doc");
        assert_eq!(run(&db, QUERY).rows, combined);
    }
}

#[cfg(feature = "test-hooks")]
#[test]
fn rejected_commit_does_not_publish_speculative_statistics() {
    backends(|db| {
        setup(db);
        let before = run(db, QUERY).rows;
        let tx = db.multi_transaction(true);
        tx_run(&tx, "?[id] <- [['c']] :rm doc {id}");
        db.fail_next_commit_for_tests();
        assert!(tx.commit().is_err());
        drop(tx);
        assert_eq!(run(db, QUERY).rows, before);
    });
}
