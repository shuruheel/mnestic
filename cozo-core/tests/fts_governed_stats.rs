/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! PR #69's ranking regression through the 0.19 host-owned transaction API.
//! The three-document corpus is adapted from GrumpyChubbyCat's MPL-2.0 fixture
//! at cf0dc189f4d7d75eec7d2d955933f690882c62a1.
#![cfg(not(target_arch = "wasm32"))]
#![allow(clippy::field_reassign_with_default)]

use cozo::{
    build_hybrid_query, DbInstance, GovernedTransactionOptions, HybridSearch, ScriptMutability,
};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

const SCORES: &str = "?[id,score] := ~doc:idx{id | query:'alpha',k:20,bind_score:score} :order id";
const IDS: &str = "?[id] := *doc{id} :order id";

#[derive(Clone, Copy, Debug)]
enum End {
    Abort,
    QueryError,
    Cancel,
    Drop,
    Commit,
    #[cfg(feature = "test-hooks")]
    FailedCommit,
}

fn exercise(backend: &str, end: End) {
    let dir = tempfile::tempdir().unwrap();
    let db = DbInstance::new(backend, dir.path().join("db").to_str().unwrap(), "").unwrap();
    let run = |script: &str| {
        db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
            .unwrap()
            .rows
    };
    run(":create doc {id:String => body:String, emb:<F32;2>}");
    run("::fts create doc:idx {extractor:body,tokenizer:Simple,filters:[Lowercase]}");
    run(&format!(
        "?[id,body,emb] <- [['a','alpha alpha {}',[1,0]],['b','alpha pad pad',[0,1]],['c','{}',[2,2]]] :put doc {{id=>body,emb}}",
        ["pad"; 8].join(" "), ["pad"; 100].join(" ")
    ));
    run("::hnsw create doc:vec {fields:[emb],dim:2,dtype:F32,distance:L2,m:16,ef_construction:20}");
    let mut hybrid = HybridSearch::default();
    hybrid.relation = "doc".into();
    hybrid.vector_index = Some("vec".into());
    hybrid.query_vector = vec![1.0, 0.0];
    hybrid.vector_k = 3;
    hybrid.fts_index = Some("idx".into());
    hybrid.query_text = "alpha".into();
    hybrid.fts_k = 1;
    hybrid.detailed = true;
    hybrid.limit = 3;
    let before = run(SCORES);
    let before_hybrid = db.hybrid_search(&hybrid).unwrap().rows;
    let before_ids = run(IDS);
    let options = GovernedTransactionOptions::new(Instant::now() + Duration::from_secs(10));
    let (mut tx, worker) = db.governed_transaction(true, options).unwrap();
    let worker = std::thread::spawn(move || worker.run());
    tx.run_script("?[id] <- [['c']] :rm doc {id}", BTreeMap::new())
        .unwrap();
    let pending = tx.run_script(SCORES, BTreeMap::new()).unwrap().rows;
    assert_ne!(pending, before);
    let (script, params) = build_hybrid_query(&hybrid).unwrap();
    let pending_hybrid = tx.run_script(&script, params).unwrap().rows;
    assert_ne!(pending_hybrid, before_hybrid);
    match end {
        End::Abort => tx.abort().unwrap(),
        End::QueryError => {
            let error = tx
                .run_script("?[id] := *missing_relation{id}", BTreeMap::new())
                .unwrap_err();
            assert_eq!(
                error.code().unwrap().to_string(),
                "query::relation_not_found"
            );
            drop(tx);
        }
        End::Cancel => {
            tx.cancellation().cancel();
            drop(tx);
        }
        End::Drop => drop(tx),
        End::Commit => tx.commit().unwrap(),
        #[cfg(feature = "test-hooks")]
        End::FailedCommit => {
            db.fail_next_commit_for_tests();
            assert!(tx.commit().is_err());
        }
    }
    worker.join().unwrap();
    if matches!(end, End::Commit) {
        assert_eq!(run(SCORES), pending, "{backend}: {end:?}");
        assert_eq!(db.hybrid_search(&hybrid).unwrap().rows, pending_hybrid);
        assert_eq!(run(IDS).len(), 2);
    } else {
        assert_eq!(run(IDS), before_ids);
        assert_eq!(run(SCORES), before, "{backend}: {end:?}");
        assert_eq!(db.hybrid_search(&hybrid).unwrap().rows, before_hybrid);
    }
}

fn exercise_backend(backend: &str) {
    for end in [
        End::Abort,
        End::QueryError,
        End::Cancel,
        End::Drop,
        End::Commit,
    ] {
        exercise(backend, end);
    }
    #[cfg(feature = "test-hooks")]
    exercise(backend, End::FailedCommit);
}

#[test]
fn mem_governed_fts_and_hybrid_follow_transaction_outcome() {
    exercise_backend("mem");
}

#[cfg(feature = "storage-sqlite")]
#[test]
fn sqlite_governed_fts_and_hybrid_follow_transaction_outcome() {
    exercise_backend("sqlite");
}

#[cfg(feature = "storage-rocksdb")]
#[test]
fn rocks_governed_fts_and_hybrid_follow_transaction_outcome() {
    exercise_backend("rocksdb");
}
