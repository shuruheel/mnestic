/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file, You can obtain one at
 * https://mozilla.org/MPL/2.0/.
 */

use cozo::{DbInstance, NamedRows, ScriptMutability};
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn run(db: &DbInstance, script: &str) -> NamedRows {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("{script}\n{e:?}"))
}
fn rows(db: &DbInstance, script: &str) -> Value {
    run(db, script).into_json()["rows"].clone()
}
fn engines(mut test: impl FnMut(&DbInstance)) {
    let dir = tempfile::tempdir().unwrap();
    let mut engines = vec!["mem"];
    #[cfg(feature = "storage-sqlite")]
    engines.push("sqlite");
    #[cfg(feature = "storage-rocksdb")]
    engines.push("rocksdb");
    for engine in engines {
        let path = dir.path().join(engine);
        let db = DbInstance::new(engine, path.to_str().unwrap(), "{}").unwrap();
        test(&db);
    }
}
fn fts(db: &DbInstance, word: &str) -> Value {
    rows(
        db,
        &format!("?[id,body] := ~doc:text{{id,body | query:'{word}',k:50}} :order id"),
    )
}
fn all_vectors(db: &DbInstance) -> Value {
    rows(
        db,
        "?[id] := ~doc:vec{id | query:vec([0,0]),k:50,ef:100} :order id",
    )
}
fn create_doc(db: &DbInstance) {
    run(
        db,
        ":create doc {id:String,tt:TxTime => body:String,v:<F32;2>}",
    );
}
fn create_indexes(db: &DbInstance) {
    run(
        db,
        "::fts create doc:text {extractor:body,tokenizer:Simple,filters:[Lowercase]}",
    );
    run(
        db,
        "::hnsw create doc:vec {fields:[v],dim:2,m:8,dtype:F32,distance:L2,ef_construction:50}",
    );
    run(db,"::lsh create doc:similar {extractor:body,tokenizer:Simple,filters:[Lowercase],n_perm:64,target_threshold:0.4,n_gram:1}");
}

#[test]
fn current_indexes_follow_corrections_retractions_and_reassertions() {
    engines(|db| {
        create_doc(db);
        run(db,"?[id,body,v] <- [['a','old alpha',[1,0]],['b','removed beta',[0,1]]] :put doc{id=>body,v}");
        let history = run(db, "::history doc [['a']]");
        let tt_col = history.headers.iter().position(|h| h == "tt").unwrap();
        let first_tt = history.rows[0][tt_col].get_int().unwrap();
        run(
            db,
            "?[id,body,v] <- [['a','current gamma',[2,0]]] :put doc{id=>body,v}",
        );
        run(db, "?[id] <- [['b']] :rm doc{id}");
        create_indexes(db);
        assert_eq!(fts(db, "old"), json!([]));
        assert_eq!(fts(db, "removed"), json!([]));
        assert_eq!(fts(db, "current"), json!([["a", "current gamma"]]));
        assert_eq!(all_vectors(db), json!([["a"]]));
        assert_eq!(
            rows(
                db,
                &format!("?[body] := *doc{{id:'a',body @ (tt:{first_tt})}}")
            ),
            json!([["old alpha"]])
        );
        run(db,"?[id,body,v] <- [['a','latest delta',[3,0]],['b','restored beta',[0,2]]] :put doc{id=>body,v}");
        assert_eq!(fts(db, "current"), json!([]));
        assert_eq!(fts(db, "latest"), json!([["a", "latest delta"]]));
        assert_eq!(all_vectors(db), json!([["a"], ["b"]]));
        assert_eq!(
            rows(
                db,
                "?[id] := ~doc:similar{id | query:'latest delta',k:50} :order id"
            ),
            json!([["a"]])
        );
        run(db, "?[id] <- [['a'],['b']] :rm doc{id}");
        assert_eq!(all_vectors(db), json!([]));
        assert_eq!(fts(db, "latest"), json!([]));
    });
}

#[test]
fn empty_index_multi_insert_update_evict_and_rebuild() {
    engines(|db| {
        create_doc(db);
        create_indexes(db);
        run(db,"?[id,body,v] <- [['a','alpha',[0,0]],['b','beta',[1,0]],['c','gamma',[2,0]],['d','delta',[3,0]]] :put doc{id=>body,v}");
        assert_eq!(all_vectors(db), json!([["a"], ["b"], ["c"], ["d"]]));
        run(db,"?[id,body,v] <- [['a','new alpha',[20,0]],['b','new beta',[21,0]],['c','new gamma',[22,0]]] :put doc{id=>body,v}");
        run(db, "::evict doc [['a'],['b']]");
        assert_eq!(all_vectors(db), json!([["c"], ["d"]]));
        assert_eq!(fts(db, "alpha"), json!([]));
        assert_eq!(rows(db, "::history doc [['a']]"), json!([]));
        let before = rows(
            db,
            "?[id,score] := ~doc:text{id | query:'gamma',k:50,bind_score:score} :order id",
        );
        run(db, "::reindex doc");
        assert_eq!(all_vectors(db), json!([["c"], ["d"]]));
        assert_eq!(
            rows(
                db,
                "?[id,score] := ~doc:text{id | query:'gamma',k:50,bind_score:score} :order id"
            ),
            before
        );
        run(db, "::index drop doc:text");
        run(
            db,
            "?[id,body,v] <- [['c','after drop',[9,9]]] :put doc{id=>body,v}",
        );
        assert_eq!(all_vectors(db), json!([["c"], ["d"]]));
    });
}

#[test]
fn historical_search_and_tt_index_inputs_are_rejected() {
    engines(|db| {
        create_doc(db);
        create_indexes(db);
        for q in [
            "?[id] := ~doc:text{id | query:'alpha',k:10} :as_of 1",
            "?[id] := ~doc:vec{id | query:vec([0,0]),k:10,ef:20} :as_of 1",
            "?[id] := ~doc:similar{id | query:'alpha',k:10} :as_of 1",
            "?[word] := *doc:text{word} :as_of 1",
            "?[rank,word] <~ ReorderSort(*doc:text[word,src_id,offset_from,offset_to,position,total_length], out:[word]) :as_of 1",
        ] {
            let error = db.run_script(q,BTreeMap::new(),ScriptMutability::Mutable).unwrap_err();
            assert!(format!("{error:?}").contains("search::as_of_unsupported"),"{q}: {error:?}");
        }
        for q in [
            "::fts create doc:bad {extractor:to_string(tt),tokenizer:Simple}",
            "::lsh create doc:bad {extractor:to_string(tt),tokenizer:Simple,n_perm:64,target_threshold:0.4,n_gram:1}",
            "::hnsw create doc:bad {fields:[tt],dim:2,m:8,dtype:F32,distance:L2,ef_construction:50}",
            "::hnsw create doc:bad {fields:[v],dim:2,m:8,dtype:F32,distance:L2,ef_construction:50,filter:tt==tt}",
        ] {
            let error = db.run_script(q,BTreeMap::new(),ScriptMutability::Mutable).unwrap_err();
            assert!(format!("{error:?}").contains("eval::txtime_index_column"),"{q}: {error:?}");
        }
        let error = db
            .run_script(
                "::index create doc:secondary {body}",
                BTreeMap::new(),
                ScriptMutability::Mutable,
            )
            .unwrap_err();
        assert!(format!("{error:?}").contains("eval::txtime_unsupported_op"));
        run(
            db,
            ":create bt {id:String,vt:Validity,tt:TxTime => body:String}",
        );
        let error = db
            .run_script(
                "::fts create bt:bad {extractor:body,tokenizer:Simple}",
                BTreeMap::new(),
                ScriptMutability::Mutable,
            )
            .unwrap_err();
        assert!(format!("{error:?}").contains("eval::txtime_unsupported_op"));
    });
}

#[test]
fn index_failure_rolls_back_base_and_other_indexes() {
    engines(|db| {
        create_doc(db);
        run(
            db,
            "?[id,body,v] <- [['a','safe alpha',[0,0]]] :put doc{id=>body,v}",
        );
        run(
            db,
            "::fts create doc:text {extractor:body,tokenizer:Simple,filters:[Lowercase]}",
        );
        run(db,"::hnsw create doc:vec {fields:[v],dim:2,m:8,dtype:F32,distance:L2,ef_construction:50,filter:assert(body != 'fail')}");
        let before = rows(db, "::history doc [['a']]");
        let error = db
            .run_script(
                "?[id,body,v] <- [['a','fail',[1,1]]] :put doc{id=>body,v}",
                BTreeMap::new(),
                ScriptMutability::Mutable,
            )
            .unwrap_err();
        assert!(format!("{error:?}").contains("assert"));
        assert_eq!(rows(db, "::history doc [['a']]"), before);
        assert_eq!(fts(db, "alpha"), json!([["a", "safe alpha"]]));
        assert_eq!(all_vectors(db), json!([["a"]]));
        run(
            db,
            "?[id,body,v] <- [['a','safe beta',[2,2]]] :put doc{id=>body,v}",
        );
        assert_eq!(fts(db, "beta"), json!([["a", "safe beta"]]));
    });
}

#[test]
fn buffered_writes_are_invisible_until_commit_and_failed_handles_cannot_retry() {
    engines(|db| {
        create_doc(db);
        create_indexes(db);
        run(
            db,
            "?[id,body,v] <- [['a','old alpha',[0,0]]] :put doc{id=>body,v}",
        );
        let tx = db.multi_transaction(true);
        tx.run_script(
            "?[id,body,v] <- [['a','new beta',[1,1]]] :put doc{id=>body,v}",
            BTreeMap::new(),
        )
        .unwrap();
        let visible = tx
            .run_script(
                "?[body] := ~doc:text{body | query:'old',k:10}",
                BTreeMap::new(),
            )
            .unwrap();
        assert_eq!(visible.into_json()["rows"], json!([["old alpha"]]));
        let base = tx
            .run_script("?[body] := *doc{id:'a',body}", BTreeMap::new())
            .unwrap();
        assert_eq!(base.into_json()["rows"], json!([["old alpha"]]));
        tx.commit().unwrap();
        assert_eq!(fts(db, "old"), json!([]));
        assert_eq!(fts(db, "new"), json!([["a", "new beta"]]));
        run(db, "::index drop doc:vec");
        run(db, "::hnsw create doc:vec {fields:[v],dim:2,m:8,dtype:F32,distance:L2,ef_construction:50,filter:assert(body != 'must rollback')}");
        let tx = db.multi_transaction(true);
        tx.run_script(
            "?[id,body,v] <- [['a','must rollback',[2,2]]] :put doc{id=>body,v}",
            BTreeMap::new(),
        )
        .unwrap();
        assert!(tx.commit().is_err());
        assert!(tx.commit().is_err());
        assert!(tx.run_script("?[x] <- [[1]]", BTreeMap::new()).is_err());
        let _ = tx.abort();
        assert_eq!(fts(db, "new"), json!([["a", "new beta"]]));
        assert_eq!(fts(db, "rollback"), json!([]));
    });
}

#[test]
fn imperative_ddl_cannot_bypass_publication_and_import_maintains_indexes() {
    engines(|db| {
        create_doc(db);
        assert!(run(db, "::reindex doc").rows[0][0]
            .get_str()
            .unwrap()
            .contains("nothing to rebuild"));
        create_indexes(db);
        for command in [
            "::reindex doc",
            "::index drop doc:text",
            "::rename doc -> renamed",
            "::remove doc",
            "::history_gc doc 0",
            "::repair_corrupt doc",
        ] {
            let script = format!("{{ ?[id,body,v] <- [['a','uncommitted',[0,0]]] :put doc{{id=>body,v}} }} {{ {command} }}");
            let error = db
                .run_script(&script, BTreeMap::new(), ScriptMutability::Mutable)
                .unwrap_err();
            assert!(
                format!("{error:?}").contains("txtime_ddl_standalone"),
                "{error:?}"
            );
            assert_eq!(rows(db, "?[id] := *doc{id}"), json!([]));
        }
        db.import_relations(BTreeMap::from([(
            "doc".to_owned(),
            NamedRows::new(
                vec!["id".into(), "body".into(), "v".into()],
                vec![vec![
                    "a".into(),
                    "imported alpha".into(),
                    cozo::DataValue::List(vec![0.into(), 0.into()]),
                ]],
            ),
        )]))
        .unwrap();
        assert_eq!(fts(db, "imported"), json!([["a", "imported alpha"]]));
        assert_eq!(all_vectors(db), json!([["a"]]));
        let error = db
            .run_script(
                "::repair_corrupt doc",
                BTreeMap::new(),
                ScriptMutability::Mutable,
            )
            .unwrap_err();
        assert!(
            format!("{error:?}").contains("drop transaction-time search indexes"),
            "{error:?}"
        );
    });
}

#[test]
fn eviction_limit_is_checked_before_any_indexed_deletion() {
    engines(|db| {
        create_doc(db);
        create_indexes(db);
        run(
            db,
            "?[id,body,v] <- [['0','retained',[0,0]]] :put doc{id=>body,v}",
        );
        let keys = (0..1001)
            .map(|i| format!("['{i}']"))
            .collect::<Vec<_>>()
            .join(",");
        let error = db
            .run_script(
                &format!("::evict doc [{keys}]"),
                BTreeMap::new(),
                ScriptMutability::Mutable,
            )
            .unwrap_err();
        assert!(
            format!("{error:?}").contains("evict_batch_limit"),
            "{error:?}"
        );
        assert_eq!(fts(db, "retained"), json!([["0", "retained"]]));
    });
}

#[test]
fn reconcile_and_duplicate_writes_keep_only_current_documents() {
    engines(|db| {
        create_doc(db);
        create_indexes(db);
        run(db, "{ ?[id,body,v] <- [['a','first',[0,0]]] :put doc{id=>body,v} } { ?[id,body,v] <- [['a','final alpha',[1,1]],['b','remove beta',[2,2]]] :put doc{id=>body,v} }");
        assert_eq!(fts(db, "first"), json!([]));
        assert_eq!(fts(db, "final"), json!([["a", "final alpha"]]));
        run(db, "?[id,body,v] <- [['a','reconciled alpha',[3,3]],['c','reconciled gamma',[4,4]]] :reconcile doc{id=>body,v}");
        assert_eq!(fts(db, "remove"), json!([]));
        assert_eq!(all_vectors(db), json!([["a"], ["c"]]));
        assert_eq!(
            fts(db, "reconciled"),
            json!([["a", "reconciled alpha"], ["c", "reconciled gamma"]])
        );
    });
}

#[cfg(feature = "storage-sqlite")]
#[test]
fn backup_restore_and_reopen_preserve_current_search_and_future_updates() {
    let dir = tempfile::tempdir().unwrap();
    let mut engines = vec!["sqlite"];
    #[cfg(feature = "storage-rocksdb")]
    engines.push("rocksdb");
    for engine in engines {
        let source = dir.path().join(format!("{engine}-source"));
        let backup = dir.path().join(format!("{engine}-backup"));
        let restored = dir.path().join(format!("{engine}-restored"));
        {
            let db = DbInstance::new(engine, source.to_str().unwrap(), "{}").unwrap();
            create_doc(&db);
            create_indexes(&db);
            run(
                &db,
                "?[id,body,v] <- [['a','old',[0,0]]] :put doc{id=>body,v}",
            );
            run(
                &db,
                "?[id,body,v] <- [['a','current alpha',[1,1]]] :put doc{id=>body,v}",
            );
            db.backup_db(backup.to_str().unwrap()).unwrap();
        }
        for (path, restore) in [(&source, false), (&restored, true)] {
            let db = DbInstance::new(engine, path.to_str().unwrap(), "{}").unwrap();
            if restore {
                db.restore_backup(backup.to_str().unwrap()).unwrap();
            }
            assert_eq!(fts(&db, "old"), json!([]));
            assert_eq!(fts(&db, "current"), json!([["a", "current alpha"]]));
            assert_eq!(all_vectors(&db), json!([["a"]]));
            run(
                &db,
                "?[id,body,v] <- [['a','updated beta',[2,2]]] :put doc{id=>body,v}",
            );
            assert_eq!(fts(&db, "current"), json!([]));
            assert_eq!(fts(&db, "updated"), json!([["a", "updated beta"]]));
        }
    }
}

#[test]
fn rename_moves_attached_search_catalogs_and_embedded_handles() {
    engines(|db| {
        create_doc(db);
        create_indexes(db);
        run(
            db,
            "?[id,body,v] <- [['a','alpha',[0,0]]] :put doc{id=>body,v}",
        );
        run(db, "::rename doc -> renamed");
        assert_eq!(
            rows(db, "?[id] := ~renamed:text{id | query:'alpha',k:10}"),
            json!([["a"]])
        );
        run(
            db,
            "?[id,body,v] <- [['a','beta',[1,1]]] :put renamed{id=>body,v}",
        );
        run(db, "::reindex renamed");
        assert_eq!(
            rows(db, "?[id] := ~renamed:text{id | query:'beta',k:10}"),
            json!([["a"]])
        );
        assert_eq!(
            rows(
                db,
                "?[id] := ~renamed:vec{id | query:vec([1,1]),k:10,ef:50}"
            ),
            json!([["a"]])
        );
        assert_eq!(
            rows(db, "?[id] := ~renamed:similar{id | query:'beta',k:10}"),
            json!([["a"]])
        );
        for name in ["text", "vec", "similar"] {
            run(db, &format!("::index drop renamed:{name}"));
        }
        run(db, "::remove renamed");
        create_doc(db);
        create_indexes(db);
    });
}
