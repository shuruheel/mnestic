/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Correctness guard for the mnestic fork's in-RAM HNSW build path
//! (`create_hnsw_index` builds into the temp store, then migrates). Builds an
//! index over points on a line and checks that nearest-neighbour queries agree
//! with brute-force ordering — i.e. the migrated graph is queryable and correct.
//! Uses the sqlite backend (real stored-relation path).

use cozo::{DbInstance, NamedRows, ScriptMutability};
use std::collections::BTreeMap;

fn run(db: &DbInstance, s: &str) -> NamedRows {
    db.run_script(s, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("script failed: {e:?}\n--- script ---\n{s}"))
}

#[test]
fn build_then_query_is_correct() {
    let dir = tempfile::tempdir().unwrap();
    let db = DbInstance::new("sqlite", dir.path().join("b.db").to_str().unwrap(), "").unwrap();
    run(&db, ":create pts { id: Int => emb: <F32; 2> }");

    // 200 points along the x-axis at x = id; nearest neighbours of x=q are the
    // ids closest to q. A non-trivial size so the build does real graph work.
    let rows: Vec<String> = (0..200).map(|i| format!("[{i},[{}.0,0.0]]", i)).collect();
    run(
        &db,
        &format!("?[id, emb] <- [{}] :put pts {{ id => emb }}", rows.join(",")),
    );
    run(
        &db,
        "::hnsw create pts:idx { dim: 2, m: 16, dtype: F32, fields: [emb], distance: L2, ef_construction: 50 }",
    );

    // Query near x=100; expect ids clustered around 100.
    let res = run(
        &db,
        "?[id, dist] := ~pts:idx{ id | query: vec([100.0, 0.0]), k: 5, ef: 80, bind_distance: dist } :order dist",
    );
    let ids: Vec<i64> = res.rows.iter().map(|r| r[0].get_int().unwrap()).collect();
    assert_eq!(ids.len(), 5, "expected 5 neighbours, got {ids:?}");
    assert_eq!(ids[0], 100, "nearest to x=100 must be id 100; got {ids:?}");
    // All five must be within the immediate neighbourhood (recall, not exact ANN).
    for id in &ids {
        assert!(
            (*id - 100).abs() <= 4,
            "neighbour {id} too far from 100; got {ids:?}"
        );
    }
}
