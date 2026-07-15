/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Release gate for the continuous-write claim: create FTS and HNSW indexes over an empty
//! persistent relation, keep writing after index creation and across a reopen, and prove that
//! both indexes remain queryable without a repair/rebuild step.

use cozo::{DbInstance, NamedRows, ScriptMutability};
use std::collections::BTreeMap;
use std::path::Path;

fn open(path: &Path) -> DbInstance {
    DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap()
}

fn run(db: &DbInstance, script: &str) -> NamedRows {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|error| panic!("script failed: {error:?}\n--- script ---\n{script}"))
}

fn vector(id: usize) -> String {
    let x = id as f64 + 1.0;
    format!(
        "vec([{:.3}, {:.3}, {:.3}, {:.3}])",
        x,
        (x * x) % 97.0,
        (x * 7.0) % 53.0,
        (x * 13.0) % 31.0
    )
}

fn put_range(db: &DbInstance, start: usize, end: usize) {
    for id in start..end {
        let emb = vector(id);
        run(
            db,
            &format!(
                "?[id, body, emb] <- [[{id}, 'memory marker{id}', {emb}]] \
                 :put docs {{id => body, emb}}"
            ),
        );

        if (id + 1) % 8 == 0 {
            let fts = run(
                db,
                "?[id] := ~docs:text{id | query: 'memory', k: 100} :order id",
            );
            assert_eq!(
                fts.rows.len(),
                id + 1,
                "FTS missed an incrementally written document at checkpoint {}",
                id + 1
            );

            let hnsw = run(
                db,
                &format!(
                    "?[id, distance] := ~docs:vector{{id | query: {emb}, k: 5, ef: 128, \
                     bind_distance: distance}}"
                ),
            );
            assert!(
                hnsw.rows.iter().any(|row| {
                    row[0].get_int() == Some(id as i64)
                        && row[1].get_float().is_some_and(|distance| distance == 0.0)
                }),
                "HNSW missed the exact incrementally written vector {id}: {:?}",
                hnsw.rows
            );
        }
    }
}

#[test]
fn persistent_indexes_track_writes_after_creation_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("continuous-writes.db");

    {
        let db = open(&path);
        run(&db, ":create docs {id: Int => body: String, emb: <F32; 4>}");
        run(
            &db,
            "::fts create docs:text { extractor: body, tokenizer: Simple, filters: [Lowercase] }",
        );
        run(
            &db,
            "::hnsw create docs:vector { dim: 4, m: 16, dtype: F32, fields: [emb], \
             distance: L2, ef_construction: 128 }",
        );
        put_range(&db, 0, 32);
    }

    let db = open(&path);
    put_range(&db, 32, 64);

    for id in 0..16 {
        let emb = vector(id);
        run(
            &db,
            &format!(
                "?[id, body, emb] <- [[{id}, 'retired marker{id}', {emb}]] \
                 :put docs {{id => body, emb}}"
            ),
        );
    }

    let active = run(
        &db,
        "?[id] := ~docs:text{id | query: 'memory', k: 100} :order id",
    );
    assert_eq!(
        active.rows.len(),
        48,
        "FTS retained stale replacement postings"
    );
    assert!(
        active
            .rows
            .iter()
            .all(|row| row[0].get_int().unwrap() >= 16),
        "FTS returned a replaced document: {:?}",
        active.rows
    );

    let retired = run(
        &db,
        "?[id] := ~docs:text{id | query: 'retired', k: 100} :order id",
    );
    assert_eq!(retired.rows.len(), 16);

    for id in [0, 15, 31, 47, 63] {
        let hnsw = run(
            &db,
            &format!(
                "?[id, distance] := ~docs:vector{{id | query: {}, k: 5, ef: 128, \
                 bind_distance: distance}}",
                vector(id)
            ),
        );
        assert!(
            hnsw.rows.iter().any(|row| {
                row[0].get_int() == Some(id as i64)
                    && row[1].get_float().is_some_and(|distance| distance == 0.0)
            }),
            "HNSW lost incrementally written vector {id}: {:?}",
            hnsw.rows
        );
    }
}
