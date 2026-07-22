/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Correctness guards for the mnestic fork's non-blocking HNSW build
//! (`Storage::ingest_sorted` SST publish + off-lock build). These run on the
//! **rocksdb** backend specifically, because the SST ingest path
//! (`SstFileWriter`/`IngestExternalFile`) only exists there — the sqlite/mem
//! guards in `hnsw_build.rs` exercise the per-key flush fallback.

#![cfg(feature = "storage-rocksdb")]

use cozo::{DbInstance, NamedRows, ScriptMutability};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

fn run(db: &DbInstance, s: &str) -> NamedRows {
    db.run_script(s, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("script failed: {e:?}\n--- script ---\n{s}"))
}

fn put_line_points(db: &DbInstance, n: i64) {
    let rows: Vec<String> = (0..n).map(|i| format!("[{i},[{}.0,0.0]]", i)).collect();
    run(
        db,
        &format!(
            "?[id, emb] <- [{}] :put pts {{ id => emb }}",
            rows.join(",")
        ),
    );
}

fn nearest_ids(db: &DbInstance, q: f64, k: usize) -> Vec<i64> {
    let res = run(
        db,
        &format!(
            "?[id, dist] := ~pts:idx{{ id | query: vec([{q}, 0.0]), k: {k}, ef: 80, bind_distance: dist }} :order dist"
        ),
    );
    res.rows.iter().map(|r| r[0].get_int().unwrap()).collect()
}

/// The index built via the SST-ingest publish path must be queryable and
/// correct — i.e. the bulk-loaded graph matches what the per-key path produced.
#[test]
fn sst_built_index_is_correct() {
    let dir = tempfile::tempdir().unwrap();
    let db = DbInstance::new("rocksdb", dir.path().join("db").to_str().unwrap(), "").unwrap();
    run(&db, ":create pts { id: Int => emb: <F32; 2> }");
    put_line_points(&db, 200);
    run(
        &db,
        "::hnsw create pts:idx { dim: 2, m: 16, dtype: F32, fields: [emb], distance: L2, ef_construction: 50 }",
    );

    let ids = nearest_ids(&db, 100.0, 5);
    assert_eq!(ids.len(), 5, "expected 5 neighbours, got {ids:?}");
    assert_eq!(ids[0], 100, "nearest to x=100 must be id 100; got {ids:?}");
    for id in &ids {
        assert!(
            (*id - 100).abs() <= 4,
            "neighbour {id} too far from 100; got {ids:?}"
        );
    }
}

/// SST-ingested keys must be genuinely persisted in the live store (not just
/// living in the build transaction's overlay): closing and reopening the DB and
/// querying the index must still return the bulk-loaded graph.
#[test]
fn sst_built_index_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let path_str = path.to_str().unwrap();
    {
        let db = DbInstance::new("rocksdb", path_str, "").unwrap();
        run(&db, ":create pts { id: Int => emb: <F32; 2> }");
        put_line_points(&db, 200);
        run(
            &db,
            "::hnsw create pts:idx { dim: 2, m: 16, dtype: F32, fields: [emb], distance: L2, ef_construction: 50 }",
        );
        // drop closes the DB
    }

    let db = DbInstance::new("rocksdb", path_str, "").unwrap();
    let ids = nearest_ids(&db, 50.0, 5);
    assert_eq!(ids.len(), 5, "index lost across reopen; got {ids:?}");
    assert_eq!(
        ids[0], 50,
        "nearest to x=50 must be id 50 after reopen; got {ids:?}"
    );
    for id in &ids {
        assert!(
            (*id - 50).abs() <= 4,
            "neighbour {id} too far from 50 after reopen; got {ids:?}"
        );
    }
}

/// The core reader-unblock guarantee: while a (deliberately large) index build
/// runs on a relation, concurrent reads of that same relation must keep
/// completing — not queue behind the build for its whole duration. With the old
/// in-lock build, the reader would acquire the relation read-lock only after the
/// build released its write-lock minutes later, so very few reads would get
/// through; off-lock, many do.
#[test]
fn reads_proceed_during_build() {
    let dir = tempfile::tempdir().unwrap();
    let db = DbInstance::new("rocksdb", dir.path().join("db").to_str().unwrap(), "").unwrap();
    run(&db, ":create pts { id: Int => emb: <F32; 2> }");
    put_line_points(&db, 6000);

    let done = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            run(
                &db,
                "::hnsw create pts:idx { dim: 2, m: 16, dtype: F32, fields: [emb], distance: L2, ef_construction: 50 }",
            );
            done.store(true, Ordering::SeqCst);
        });

        // Hammer the same relation with reads while the build runs.
        let mut reads_during_build = 0u64;
        while !done.load(Ordering::SeqCst) {
            // A plain scan+filter read of the same relation the build is on.
            let res = run(&db, "?[id] := *pts{id, emb: _}, id < 10");
            assert_eq!(res.rows.len(), 10, "concurrent read returned wrong rows");
            reads_during_build += 1;
        }
        assert!(
            reads_during_build >= 5,
            "only {reads_during_build} reads completed during the build — reads appear to be \
             blocked by the index build"
        );
    });
}

/// Rows inserted *during* the unlocked build window must still end up in the
/// finished index — whether they land before the build snapshot (bulk-built),
/// during the build (Phase-D reconcile), or after publication (steady-state
/// maintenance). Exercises the reconcile path end-to-end.
#[test]
fn concurrent_inserts_during_build_are_all_indexed() {
    let dir = tempfile::tempdir().unwrap();
    let db = DbInstance::new("rocksdb", dir.path().join("db").to_str().unwrap(), "").unwrap();
    run(&db, ":create pts { id: Int => emb: <F32; 2> }");
    put_line_points(&db, 5000);

    // ids 10000..10050 sit at x = id; far from the original 0..5000 cluster so a
    // query at their location must return *them* if they were indexed.
    let extra: Vec<i64> = (10000..10050).collect();

    std::thread::scope(|s| {
        s.spawn(|| {
            run(
                &db,
                "::hnsw create pts:idx { dim: 2, m: 16, dtype: F32, fields: [emb], distance: L2, ef_construction: 50 }",
            );
        });
        // Insert the extra points one at a time so some land mid-build.
        for &id in &extra {
            run(
                &db,
                &format!("?[id, emb] <- [[{id},[{id}.0,0.0]]] :put pts {{ id => emb }}"),
            );
        }
    });

    // Every concurrently-inserted point must be findable as its own nearest
    // neighbour — i.e. it made it into the index by one path or another.
    for &id in &extra {
        let ids = nearest_ids(&db, id as f64, 1);
        assert_eq!(
            ids.first().copied(),
            Some(id),
            "concurrently-inserted id {id} is missing from the index (got {ids:?})"
        );
    }
    // And an original point is still correctly indexed.
    let ids = nearest_ids(&db, 2500.0, 1);
    assert_eq!(ids.first().copied(), Some(2500));
}

/// Drop + recreate (the "rebuild" path, since cozo has no in-place rebuild): the
/// new index gets a fresh relation id, sees rows added after the drop, and
/// queries correctly.
#[test]
fn drop_and_recreate_index() {
    let dir = tempfile::tempdir().unwrap();
    let db = DbInstance::new("rocksdb", dir.path().join("db").to_str().unwrap(), "").unwrap();
    run(&db, ":create pts { id: Int => emb: <F32; 2> }");
    put_line_points(&db, 200);
    run(
        &db,
        "::hnsw create pts:idx { dim: 2, m: 16, dtype: F32, fields: [emb], distance: L2, ef_construction: 50 }",
    );
    assert_eq!(nearest_ids(&db, 100.0, 1).first().copied(), Some(100));

    run(&db, "::hnsw drop pts:idx");
    // Add points only present for the rebuild.
    run(
        &db,
        "?[id, emb] <- [[300,[300.0,0.0]],[301,[301.0,0.0]]] :put pts { id => emb }",
    );
    run(
        &db,
        "::hnsw create pts:idx { dim: 2, m: 16, dtype: F32, fields: [emb], distance: L2, ef_construction: 50 }",
    );

    assert_eq!(nearest_ids(&db, 100.0, 1).first().copied(), Some(100));
    assert_eq!(
        nearest_ids(&db, 300.0, 1).first().copied(),
        Some(300),
        "rebuilt index missing a row added after the drop"
    );
}

/// Not a correctness test — a measurement harness for the changelog. Builds a
/// large index and reports how long a concurrent read of the same relation
/// takes versus the whole build. Run with:
/// `cargo test --features storage-rocksdb --test hnsw_nonblocking_build -- --ignored --nocapture measure`
#[test]
#[ignore]
fn measure_reader_unblock() {
    let n: i64 = std::env::var("MEASURE_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40000);
    let dir = tempfile::tempdir().unwrap();
    let db = DbInstance::new("rocksdb", dir.path().join("db").to_str().unwrap(), "").unwrap();
    run(&db, ":create pts { id: Int => emb: <F32; 2> }");
    put_line_points(&db, n);

    let done = AtomicBool::new(false);
    let mut max_read_ms = 0f64;
    let mut reads = 0u64;
    let build_elapsed = std::thread::scope(|s| {
        let h = s.spawn(|| {
            let t = Instant::now();
            run(
                &db,
                "::hnsw create pts:idx { dim: 2, m: 16, dtype: F32, fields: [emb], distance: L2, ef_construction: 50 }",
            );
            done.store(true, Ordering::SeqCst);
            t.elapsed().as_secs_f64()
        });
        while !done.load(Ordering::SeqCst) {
            let t = Instant::now();
            let _ = run(&db, "?[id] := *pts{id, emb: _}, id < 10");
            max_read_ms = max_read_ms.max(t.elapsed().as_secs_f64() * 1000.0);
            reads += 1;
        }
        h.join().unwrap()
    });

    println!(
        "MEASURE n={n}: build={build_elapsed:.2}s, {reads} concurrent reads completed during it, \
         slowest concurrent read={max_read_ms:.1}ms"
    );
}
