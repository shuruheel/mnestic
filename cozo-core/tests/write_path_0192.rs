// SPDX-License-Identifier: MPL-2.0
//! 0.19.2 write-path and filtered-search regressions.

use std::collections::BTreeMap;

use cozo::{DbInstance, NamedRows, ScriptMutability};

fn run(db: &DbInstance, script: &str) -> NamedRows {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("{script}: {e:?}"))
}

fn persistent(mut test: impl FnMut(&dyn Fn() -> DbInstance)) {
    #[cfg(feature = "storage-sqlite")]
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite");
        test(&|| DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap());
    }
    #[cfg(feature = "storage-rocksdb")]
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        test(&|| DbInstance::new("rocksdb", path.to_str().unwrap(), "").unwrap());
    }
}

const QUERY: &str = "?[id,score] := ~doc:idx{id | query:'alpha',k:20,bind_score:score} :order id";

#[test]
fn first_write_after_reopen_does_not_scan_the_corpus() {
    persistent(|open| {
        {
            let db = open();
            run(&db, ":create doc {id:String => body:String}");
            run(
                &db,
                "::fts create doc:idx {extractor:body,tokenizer:Simple,filters:[Lowercase]}",
            );
            run(
                &db,
                "?[id,body] <- [['a','alpha alpha pad'],['b','alpha pad pad pad'],['c','beta']] \
                 :put doc {id=>body}",
            );
        }
        let db = open();
        cozo::FTS_DOC_STATS_SCANS.with(|n| n.set(0));
        // Update, insert and delete: none may seed corpus totals.
        run(
            &db,
            "?[id,body] <- [['b','alpha alpha pad']] :put doc {id=>body}",
        );
        run(
            &db,
            "?[id,body] <- [['d','alpha gamma']] :put doc {id=>body}",
        );
        run(&db, "?[id] <- [['c']] :rm doc {id}");
        assert_eq!(cozo::FTS_DOC_STATS_SCANS.with(|n| n.get()), 0);
        // The first search seeds them, and its scores match a fresh rebuild.
        let incremental = run(&db, QUERY).rows;
        run(&db, "::reindex doc");
        assert_eq!(run(&db, QUERY).rows, incremental);
    });
}

/// Physical rows of an index relation, for before/after comparison.
fn index_rows(db: &DbInstance, index: &str, cols: &str) -> Vec<Vec<cozo::DataValue>> {
    run(db, &format!("?[{cols}] := *{index}{{{cols}}}")).rows
}

#[test]
fn unchanged_fields_keep_their_index_entries() {
    let db = DbInstance::new("mem", "", "").unwrap();
    run(&db, ":create doc {id:Int => body:String, v:<F32;2>}");
    run(
        &db,
        "::hnsw create doc:vec {dim:2,m:8,dtype:F32,fields:[v],distance:L2,ef_construction:20}",
    );
    run(
        &db,
        "::fts create doc:idx {extractor:body,tokenizer:Simple,filters:[Lowercase]}",
    );
    for i in 0..40 {
        run(
            &db,
            &format!(
                "?[id,body,v] <- [[{i},'word{} common',[{}.0,{}.0]]] :put doc {{id=>body,v}}",
                i % 7,
                i % 9,
                i / 9
            ),
        );
    }
    let graph_cols = "layer,fr_id,to_id,dist,ignore_link";
    let graph = index_rows(&db, "doc:vec", graph_cols);
    // Text-only edit: the HNSW graph is untouched.
    run(
        &db,
        "?[id,body,v] <- [[5,'edited text',[5.0,0.0]]] :put doc {id=>body,v}",
    );
    assert_eq!(index_rows(&db, "doc:vec", graph_cols), graph);
    let hits = run(&db, "?[id] := ~doc:idx{id | query:'edited',k:5}").rows;
    assert_eq!(hits.len(), 1);
    // Vector-only edit: postings are untouched; the graph does change.
    let postings = index_rows(&db, "doc:idx", "word,src_id");
    run(
        &db,
        "?[id,body,v] <- [[5,'edited text',[50.0,50.0]]] :put doc {id=>body,v}",
    );
    assert_eq!(index_rows(&db, "doc:idx", "word,src_id"), postings);
    assert_ne!(index_rows(&db, "doc:vec", graph_cols), graph);
    let near = run(
        &db,
        "?[id] := ~doc:vec{id | query: vec([50.0,50.0]), k:1, ef:20}",
    )
    .rows;
    assert_eq!(near[0][0].get_int(), Some(5));
    // `:update` takes the same path.
    run(&db, "?[id,body] <- [[5,'again']] :update doc {id=>body}");
    assert_eq!(
        run(
            &db,
            "?[id] := ~doc:vec{id | query: vec([50.0,50.0]), k:1, ef:20}"
        )
        .rows,
        near
    );
}

#[test]
fn selective_filters_return_the_exact_eligible_top_k() {
    let db = DbInstance::new("mem", "", "").unwrap();
    run(&db, ":create p {id:Int => kind:String, v:<F32;2>}");
    run(
        &db,
        "::hnsw create p:vec {dim:2,m:8,dtype:F32,fields:[v],distance:L2,ef_construction:32}",
    );
    // 1,000 rows on a spiral; every 50th is 'rare' (2%).
    for chunk in (0..1000).collect::<Vec<i64>>().chunks(200) {
        let rows: Vec<String> = chunk
            .iter()
            .map(|i| {
                let t = *i as f64 * 0.05;
                let kind = if i % 50 == 0 { "rare" } else { "common" };
                format!("[{i},'{kind}',[{:.4},{:.4}]]", t * t.cos(), t * t.sin())
            })
            .collect();
        run(
            &db,
            &format!("?[id,kind,v] <- [{}] :put p {{id=>kind,v}}", rows.join(",")),
        );
    }
    let exact = run(
        &db,
        "?[id,d] := *p{id,kind,v}, kind == 'rare', d = l2_dist(v, vec([3.0,4.0])) \
         :order d :limit 5",
    )
    .rows;
    let filtered = run(
        &db,
        "?[id,d] := ~p:vec{id,kind | query: vec([3.0,4.0]), k:5, ef:20, \
         filter: kind == 'rare', bind_distance: d} :order d",
    )
    .rows;
    let ids =
        |rows: &Vec<Vec<cozo::DataValue>>| rows.iter().map(|r| r[0].clone()).collect::<Vec<_>>();
    assert_eq!(ids(&filtered), ids(&exact));
    // Fewer eligible rows than k: all of them, not a truncated window.
    let none = run(
        &db,
        "?[id] := ~p:vec{id,kind | query: vec([3.0,4.0]), k:5, ef:20, filter: kind == 'none'}",
    );
    assert!(none.rows.is_empty());
    let few = run(
        &db,
        "?[id] := ~p:vec{id | query: vec([3.0,4.0]), k:5, ef:20, filter: id < 3}",
    );
    assert_eq!(few.rows.len(), 3);
}

#[cfg(feature = "storage-sqlite")]
#[test]
fn sqlite_defaults_to_wal_and_new_files_without_rowid() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.sqlite");
    {
        let db = DbInstance::new("sqlite", path.to_str().unwrap(), "{}").unwrap();
        run(&db, ":create t {k:Int => v:String}");
        run(&db, "?[k,v] <- [[1,'a']] :put t {k=>v}");
        assert!(dir.path().join("db.sqlite-wal").exists());
    }
    let conn = sqlite::open(&path).unwrap();
    let mut mode = String::new();
    conn.iterate("pragma journal_mode", |row| {
        mode = row[0].1.unwrap_or_default().to_string();
        true
    })
    .unwrap();
    assert_eq!(mode, "wal");
    let mut ddl = String::new();
    conn.iterate("select sql from sqlite_master where name = 'cozo'", |row| {
        ddl = row[0].1.unwrap_or_default().to_string();
        true
    })
    .unwrap();
    assert!(ddl.to_lowercase().contains("without rowid"), "{ddl}");
    drop(conn);
    // `delete` opts out, and backups stay single files either way.
    let db = DbInstance::new(
        "sqlite",
        path.to_str().unwrap(),
        r#"{"journal_mode":"delete","cache_mb":8}"#,
    )
    .unwrap();
    assert_eq!(run(&db, "?[v] := *t{k: 1, v}").rows.len(), 1);
    let backup = dir.path().join("backup.sqlite");
    db.backup_db(backup.to_str().unwrap()).unwrap();
    assert!(!dir.path().join("backup.sqlite-wal").exists());
    assert!(DbInstance::new("sqlite", path.to_str().unwrap(), r#"{"journal_mode":"x"}"#).is_err());
}
