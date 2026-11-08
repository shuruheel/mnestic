// SPDX-License-Identifier: MPL-2.0

use cozo::{DbInstance, NamedRows, ScriptMutability};

fn run(db: &DbInstance, script: &str) -> NamedRows {
    db.run_script(script, Default::default(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("{script}: {e:?}"))
}

fn engines() -> Vec<&'static str> {
    let mut ret = vec!["mem"];
    if cfg!(feature = "storage-sqlite") {
        ret.push("sqlite");
    }
    if cfg!(feature = "storage-rocksdb") {
        ret.push("rocksdb");
    }
    ret
}

fn setup(db: &DbInstance) {
    run(db, ":create p {id:Int=>v:<F32;2>?}");
    run(
        db,
        "::hnsw create p:g {fields:[v],dim:2,m:8,dtype:F32,distance:L2,ef_construction:50}",
    );
    // Serial monotonic insertion makes the level-zero bridge independent of
    // random upper levels. Assert the topology rather than relying on a seed.
    for id in 0..3 {
        run(
            db,
            &format!("?[id,v]<-[[{id},[{id},{id}]]] :put p{{id=>v}}"),
        );
    }
    assert_eq!(
        run(
            db,
            "?[a,b] := *p:g{layer:0,fr_id:a,to_id:b,ignore_link:false}, a!=b :order a,b"
        )
        .into_json()["rows"],
        serde_json::json!([[0, 1], [1, 0], [1, 2], [2, 1]])
    );
}

fn graph(db: &DbInstance) -> NamedRows {
    run(
        db,
        "?[l,a,f,s,b,g,t,d,h,i] := *p:g[l,a,f,s,b,g,t,d,h,i] :order l,a,f,s,b,g,t",
    )
}

fn assert_search(db: &DbInstance, ids: &[i64]) {
    let found = run(
        db,
        "?[id] := ~p:g{id | query:vec([0,0]),k:30,ef:50} :order id",
    );
    assert_eq!(
        found
            .rows
            .iter()
            .map(|r| r[0].get_int().unwrap())
            .collect::<Vec<_>>(),
        ids
    );
    // Every stored degree equals its number of active outgoing edges.
    let rows = run(db, "?[l,a,f,s,b,g,t,d,i] := *p:g{layer:l,fr_id:a,fr__field:f,fr__sub_idx:s,to_id:b,to__field:g,to__sub_idx:t,dist:d,ignore_link:i}, l<=0").rows;
    for row in &rows {
        if row[1..4] == row[4..7] {
            let degree = rows
                .iter()
                .filter(|r| {
                    r[0..4] == row[0..4] && r[1..4] != r[4..7] && r[8].get_bool() == Some(false)
                })
                .count();
            assert_eq!(row[7].get_float().unwrap(), degree as f64, "{row:?}");
        }
    }
}

#[test]
fn bridge_delete_and_vector_replacement_preserve_survivors() {
    for engine in engines() {
        for mutation in [
            "?[id]<-[[1]] :rm p{id}",
            "?[id,v]<-[[1,[0,0]]] :put p{id=>v}",
            "?[id,v]<-[[1,[0.5,0.5]]] :put p{id=>v}",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let db = DbInstance::new(engine, dir.path().join("db").to_str().unwrap(), "").unwrap();
            setup(&db);
            run(&db, mutation);
            assert_search(
                &db,
                if mutation.contains(":rm") {
                    &[0, 2]
                } else {
                    &[0, 1, 2]
                },
            );
        }
    }
}

#[test]
fn repair_rolls_back_with_script_and_transaction() {
    for engine in engines() {
        let dir = tempfile::tempdir().unwrap();
        let db = DbInstance::new(engine, dir.path().join("db").to_str().unwrap(), "").unwrap();
        setup(&db);
        let before = graph(&db).rows;
        assert!(db
            .run_script(
                "{?[id]<-[[1]] :rm p{id}} {?[x] := *missing_repair{x}}",
                Default::default(),
                ScriptMutability::Mutable
            )
            .is_err());
        assert_eq!(graph(&db).rows, before);
        let tx = db.multi_transaction(true);
        tx.run_script("?[id]<-[[1]] :rm p{id}", Default::default())
            .unwrap();
        assert_eq!(
            tx.run_script(
                "?[id] := ~p:g{id | query:vec([0,0]),k:30,ef:50} :order id",
                Default::default()
            )
            .unwrap()
            .rows
            .len(),
            2
        );
        tx.abort().unwrap();
        drop(tx);
        assert_eq!(graph(&db).rows, before);
        assert_search(&db, &[0, 1, 2]);
    }
}

#[test]
fn repeated_mutations_and_reopen_preserve_repaired_graph() {
    for engine in engines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let db = DbInstance::new(engine, path.to_str().unwrap(), "").unwrap();
        setup(&db);
        for i in 0..20 {
            run(&db, "?[id]<-[[1]] :rm p{id}");
            assert_search(&db, &[0, 2]);
            run(
                &db,
                &format!("?[id,v]<-[[1,[{},{}]]] :put p{{id=>v}}", i % 3, i % 3),
            );
            assert_search(&db, &[0, 1, 2]);
        }
        let before = graph(&db).rows;
        if engine != "mem" {
            drop(db);
            let reopened = DbInstance::new(engine, path.to_str().unwrap(), "").unwrap();
            assert_eq!(graph(&reopened).rows, before);
            assert_search(&reopened, &[0, 1, 2]);
        }
    }
}

#[test]
fn sibling_vector_identities_survive_list_shrink_and_row_delete() {
    for engine in engines() {
        let dir = tempfile::tempdir().unwrap();
        let db = DbInstance::new(engine, dir.path().join("db").to_str().unwrap(), "").unwrap();
        run(&db, ":create p {id:Int=>v:[<F32;2>],w:<F32;2>}");
        run(&db, "::hnsw create p:g {fields:[v,w],dim:2,m:8,dtype:F32,distance:L2,ef_construction:50,extend_candidates:true}");
        run(
            &db,
            "?[id,v,w]<-[[0,[[0,0],[1,1],[2,2]],[3,3]],[1,[[4,4]],[5,5]]] :put p{id=>v,w}",
        );
        let query =
            "?[id,f,s] := ~p:g{id | query:vec([0,0]),k:30,ef:50,bind_field:f,bind_field_idx:s}";
        assert_eq!(run(&db, query).rows.len(), 6);
        run(&db, "?[id,v,w]<-[[0,[[0,0]],[3,3]]] :put p{id=>v,w}");
        assert_eq!(run(&db, query).rows.len(), 4);
        assert_search(&db, &[0, 1]);
        run(&db, "?[id]<-[[0]] :rm p{id}");
        assert_eq!(run(&db, query).rows.len(), 2);
        assert_search(&db, &[1]);
    }
}

#[test]
fn composite_keys_and_index_predicate_removal_preserve_bridges() {
    for engine in engines() {
        let dir = tempfile::tempdir().unwrap();
        let db = DbInstance::new(engine, dir.path().join("db").to_str().unwrap(), "").unwrap();
        run(
            &db,
            ":create p {tenant:String,id:Int=>v:<F32;2>,active:Bool}",
        );
        run(&db, "::hnsw create p:g {fields:[v],dim:2,m:8,dtype:F32,distance:L2,ef_construction:50,filter:active}");
        for id in 0..3 {
            run(&db, &format!("?[tenant,id,v,active]<-[['t',{id},[{id},{id}],true]] :put p{{tenant,id=>v,active}}"));
        }
        run(
            &db,
            "?[tenant,id,v,active]<-[['t',1,[1,1],false]] :put p{tenant,id=>v,active}",
        );
        let found = run(
            &db,
            "?[tenant,id] := ~p:g{tenant,id | query:vec([0,0]),k:30,ef:50} :order id",
        );
        assert_eq!(
            found.into_json()["rows"],
            serde_json::json!([["t", 0], ["t", 2]])
        );
    }
}

#[test]
fn failed_graph_mutation_cannot_be_committed_by_legacy_client() {
    for engine in engines() {
        let dir = tempfile::tempdir().unwrap();
        let db = DbInstance::new(engine, dir.path().join("db").to_str().unwrap(), "").unwrap();
        setup(&db);
        // Deliberately emulate a stranded legacy index after an unmaintained
        // bulk import. Repair must report the missing vector, never panic.
        db.import_relations(std::collections::BTreeMap::from([(
            "p".to_string(),
            NamedRows::new(
                vec!["id".into(), "v".into()],
                vec![vec![cozo::DataValue::from(2), cozo::DataValue::Null]],
            ),
        )]))
        .unwrap();
        let before = graph(&db).rows;
        let tx = db.multi_transaction(true);
        assert!(tx
            .run_script("?[id]<-[[1]] :rm p{id}", Default::default())
            .is_err());
        let error = tx.commit().unwrap_err();
        assert!(format!("{error:?}").contains("incomplete HNSW mutation"));
        drop(tx);
        assert_eq!(graph(&db).rows, before);
        assert_eq!(run(&db, "?[id] := *p{id} :order id").rows.len(), 3);
    }
}

#[test]
fn deleting_the_actual_entry_point_and_last_node_is_safe() {
    for engine in engines() {
        let dir = tempfile::tempdir().unwrap();
        let db = DbInstance::new(engine, dir.path().join("db").to_str().unwrap(), "").unwrap();
        setup(&db);
        let mut remaining = vec![0, 1, 2];
        while !remaining.is_empty() {
            let entry = run(
                &db,
                "?[id,l] := *p:g{layer:l,fr_id:id}, l<=0 :order l,id :limit 1",
            )
            .rows[0][0]
                .get_int()
                .unwrap();
            run(&db, &format!("?[id]<-[[{entry}]] :rm p{{id}}"));
            remaining.retain(|id| *id != entry);
            assert_search(&db, &remaining);
        }
        run(&db, "?[id,v]<-[[7,[7,7]]] :put p{id=>v}");
        assert_search(&db, &[7]);
    }
}

#[test]
fn retained_twenty_five_point_mutation_matrix_has_full_tiny_coverage() {
    let mutations = [
        "?[id,kind,v]<-[[20,'common',[20,20]]] :put p{id=>kind,v}",
        "?[id,kind,v]<-[[23,'rare',[22,22]]] :put p{id=>kind,v}",
        "?[id,kind,v]<-[[23,'rare',[22.5,22.5]]] :put p{id=>kind,v}",
        "?[id]<-[[23]] :rm p{id}",
        "?[id]<-[[24]] :rm p{id}",
        "?[id,kind,v]<-[[25,'rare',[22,22]]] :put p{id=>kind,v}",
        "?[id,kind,v]<-[[20,'common',[20,20]],[23,'rare',[22,22]]] :put p{id=>kind,v}",
    ];
    for engine in engines() {
        for mutation in mutations {
            for _ in 0..10 {
                let dir = tempfile::tempdir().unwrap();
                let db =
                    DbInstance::new(engine, dir.path().join("db").to_str().unwrap(), "").unwrap();
                run(&db, ":create p {id:Int=>kind:String,v:<F32;2>}");
                let rows = (0..25)
                    .map(|id| {
                        format!(
                            "[{id},'{}',[{id},{id}]]",
                            if [20, 22, 24].contains(&id) {
                                "rare"
                            } else {
                                "common"
                            }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                run(&db, &format!("?[id,kind,v]<-[{rows}] :put p{{id=>kind,v}}"));
                run(&db,"::hnsw create p:g {fields:[v],dim:2,m:8,dtype:F32,distance:L2,ef_construction:50}");
                run(&db, mutation);
                let expected = run(&db, "?[id] := *p{id} :order id").rows;
                let rare = run(&db, "?[id] := *p{id,kind}, kind=='rare' :order id").rows;
                for ef in [50, 1600] {
                    assert_eq!(
                        run(
                            &db,
                            &format!(
                                "?[id] := ~p:g{{id | query:vec([0,0]),k:30,ef:{ef}}} :order id"
                            )
                        )
                        .rows,
                        expected,
                        "{engine}: {mutation}"
                    );
                    assert_eq!(run(&db,&format!("?[id] := ~p:g{{id,kind | query:vec([0,0]),k:30,ef:{ef},filter:kind=='rare'}} :order id")).rows,rare,"{engine}: {mutation}");
                }
                assert_search(
                    &db,
                    &expected
                        .iter()
                        .map(|r| r[0].get_int().unwrap())
                        .collect::<Vec<_>>(),
                );
            }
        }
    }
}

#[cfg(feature = "test-hooks")]
#[test]
fn failed_commit_does_not_publish_repair() {
    for engine in engines() {
        let dir = tempfile::tempdir().unwrap();
        let db = DbInstance::new(engine, dir.path().join("db").to_str().unwrap(), "").unwrap();
        setup(&db);
        let before = graph(&db).rows;
        let (mut tx, worker) = db
            .governed_transaction(
                true,
                cozo::GovernedTransactionOptions::new(
                    std::time::Instant::now() + std::time::Duration::from_secs(30),
                ),
            )
            .unwrap();
        let thread = std::thread::spawn(move || worker.run());
        tx.run_script("?[id]<-[[1]] :rm p{id}", Default::default())
            .unwrap();
        db.fail_next_commit_for_tests();
        assert!(tx.commit().is_err());
        thread.join().unwrap();
        assert_eq!(graph(&db).rows, before);
        assert_search(&db, &[0, 1, 2]);
    }
}
