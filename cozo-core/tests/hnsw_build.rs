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

use cozo::{DataValue, DbInstance, NamedRows, ScriptMutability};
use std::collections::BTreeMap;

fn run(db: &DbInstance, s: &str) -> NamedRows {
    db.run_script(s, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("script failed: {e:?}\n--- script ---\n{s}"))
}

fn run_params(db: &DbInstance, s: &str, params: BTreeMap<String, DataValue>) -> NamedRows {
    db.run_script(s, params, ScriptMutability::Mutable)
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

/// The flat build must index every element of a **list-of-vectors** column
/// (`sub_idx >= 0` branch), not just plain vector columns. Each doc holds two
/// segments in disjoint regions; a query near a doc's *second* segment must
/// return that doc — impossible if only `sub_idx == 0` (or none) were indexed.
#[test]
fn list_of_vectors_build_and_query() {
    let dir = tempfile::tempdir().unwrap();
    let db = DbInstance::new("sqlite", dir.path().join("l.db").to_str().unwrap(), "").unwrap();
    run(&db, ":create docs { id: Int => segs: [<F32; 2>] }");

    // doc i: segment 0 at [i, 0], segment 1 at [i, 100].
    let rows: Vec<String> = (0..100)
        .map(|i| format!("[{i},[[{i}.0,0.0],[{i}.0,100.0]]]"))
        .collect();
    run(
        &db,
        &format!("?[id, segs] <- [{}] :put docs {{ id => segs }}", rows.join(",")),
    );
    run(
        &db,
        "::hnsw create docs:idx { dim: 2, m: 16, dtype: F32, fields: [segs], distance: L2, ef_construction: 50 }",
    );

    for (q, label) in [("[50.0, 100.0]", "second segment"), ("[50.0, 0.0]", "first segment")] {
        let res = run(
            &db,
            &format!(
                "?[id, dist] := ~docs:idx{{ id | query: vec({q}), k: 3, ef: 60, bind_distance: dist }} :order dist :limit 1"
            ),
        );
        assert!(!res.rows.is_empty(), "no neighbours returned for {label}");
        let id = res.rows[0][0].get_int().unwrap();
        let dist = res.rows[0][1].get_float().unwrap();
        assert_eq!(id, 50, "nearest to {q} ({label}) must be doc 50, got {id}");
        assert!(
            dist < 1e-6,
            "doc 50 has a segment exactly at {q}; distance must be ~0, got {dist}"
        );
    }
}

/// F64 dtype + Cosine distance through the flat build: recall agreement
/// against brute-force cosine ordering. Guards the F64 slab variant and the
/// cosine normalisation (`dist = 1 - dot/(|a||b|)`) end to end.
#[test]
fn f64_cosine_build_recall() {
    let mut state: u64 = 0x9E3779B97F4A7C15;
    let mut next = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let raw = ((state >> 33) as f64 / (1u64 << 31) as f64) - 1.0;
        // Round to the 6 decimals we serialise, so brute force sees the exact
        // stored values.
        (raw * 1e6).round() / 1e6
    };

    const N: usize = 600;
    const DIM: usize = 8;
    const CLUSTERS: usize = 12;
    const K: usize = 10;

    // Centroids of magnitude ~4 with ±1 noise keep norms well away from zero
    // (cosine is undefined at the origin).
    let centroids: Vec<Vec<f64>> = (0..CLUSTERS)
        .map(|_| (0..DIM).map(|_| 4.0 * next()).collect())
        .collect();
    let vectors: Vec<Vec<f64>> = (0..N)
        .map(|i| {
            let c = &centroids[i % CLUSTERS];
            c.iter().map(|x| ((x + next()) * 1e6).round() / 1e6).collect()
        })
        .collect();

    let dir = tempfile::tempdir().unwrap();
    let db = DbInstance::new("sqlite", dir.path().join("c.db").to_str().unwrap(), "").unwrap();
    run(&db, &format!(":create pts {{ id: Int => emb: <F64; {DIM}> }}"));
    for (ci, rows) in vectors.chunks(200).enumerate() {
        let body: Vec<String> = rows
            .iter()
            .enumerate()
            .map(|(j, v)| {
                let vs: Vec<String> = v.iter().map(|x| format!("{x:.6}")).collect();
                format!("[{},[{}]]", ci * 200 + j, vs.join(","))
            })
            .collect();
        run(
            &db,
            &format!("?[id, emb] <- [{}] :put pts {{ id => emb }}", body.join(",")),
        );
    }
    run(
        &db,
        &format!(
            "::hnsw create pts:idx {{ dim: {DIM}, m: 16, dtype: F64, fields: [emb], \
             distance: Cosine, ef_construction: 64 }}"
        ),
    );

    let cosine = |a: &[f64], b: &[f64]| -> f64 {
        let dot: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
        let na: f64 = a.iter().map(|x| x * x).sum::<f64>().sqrt();
        let nb: f64 = b.iter().map(|x| x * x).sum::<f64>().sqrt();
        1.0 - dot / (na * nb)
    };

    let mut hits = 0usize;
    let mut total = 0usize;
    for qi in 0..25 {
        let q = &vectors[qi * 43 % N];
        let mut scored: Vec<(usize, f64)> =
            vectors.iter().map(|v| cosine(q, v)).enumerate().collect();
        scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        let truth: std::collections::HashSet<usize> =
            scored[..K].iter().map(|(i, _)| *i).collect();

        let qs: Vec<String> = q.iter().map(|x| format!("{x:.6}")).collect();
        let res = run(
            &db,
            &format!(
                "?[id, dist] := ~pts:idx{{ id | query: vec([{}]), k: {K}, ef: 64, \
                 bind_distance: dist }} :order dist :limit {K}",
                qs.join(",")
            ),
        );
        for row in res.rows.iter() {
            if truth.contains(&(row[0].get_int().unwrap() as usize)) {
                hits += 1;
            }
        }
        total += K;
    }
    let recall = hits as f64 / total as f64;
    assert!(
        recall >= 0.9,
        "F64/Cosine flat build recall too low: {recall:.3} ({hits}/{total})"
    );
}

/// Recall agreement of the parallel flat build against brute force on
/// clustered data: 2000 vectors in 16 dims, 25 queries, top-10. Guards the
/// per-node-lock insertion path (lost-backlink/duplicate-edge races showed up
/// exactly here as broken graph connectivity).
#[test]
fn parallel_build_recall_agreement() {
    // Deterministic LCG so the test needs no rand dev-dependency.
    let mut state: u64 = 0x2545F4914F6CDD1D;
    let mut next_f32 = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((state >> 33) as f32 / (1u64 << 31) as f32) - 1.0
    };

    const N: usize = 2000;
    const DIM: usize = 16;
    const CLUSTERS: usize = 20;
    const K: usize = 10;

    let centroids: Vec<Vec<f32>> = (0..CLUSTERS)
        .map(|_| (0..DIM).map(|_| 4.0 * next_f32()).collect())
        .collect();
    let vectors: Vec<Vec<f32>> = (0..N)
        .map(|i| {
            let c = &centroids[i % CLUSTERS];
            c.iter().map(|x| x + next_f32()).collect()
        })
        .collect();

    let dir = tempfile::tempdir().unwrap();
    let db = DbInstance::new("sqlite", dir.path().join("r.db").to_str().unwrap(), "").unwrap();
    run(&db, &format!(":create pts {{ id: Int => emb: <F32; {DIM}> }}"));
    for chunk in vectors.chunks(500).enumerate().collect::<Vec<_>>() {
        let (ci, rows) = chunk;
        let body: Vec<String> = rows
            .iter()
            .enumerate()
            .map(|(j, v)| {
                let vs: Vec<String> = v.iter().map(|x| format!("{x:.6}")).collect();
                format!("[{},[{}]]", ci * 500 + j, vs.join(","))
            })
            .collect();
        run(
            &db,
            &format!("?[id, emb] <- [{}] :put pts {{ id => emb }}", body.join(",")),
        );
    }
    run(
        &db,
        &format!(
            "::hnsw create pts:idx {{ dim: {DIM}, m: 16, dtype: F32, fields: [emb], \
             distance: L2, ef_construction: 64 }}"
        ),
    );

    let l2 = |a: &[f32], b: &[f32]| -> f64 {
        a.iter()
            .zip(b)
            .map(|(x, y)| ((x - y) as f64).powi(2))
            .sum()
    };

    let mut hits = 0usize;
    let mut total = 0usize;
    for qi in 0..25 {
        let q = &vectors[qi * 67 % N];
        // brute-force top-K ids
        let mut scored: Vec<(usize, f64)> = vectors.iter().map(|v| l2(q, v)).enumerate().collect();
        scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        let truth: std::collections::HashSet<usize> =
            scored[..K].iter().map(|(i, _)| *i).collect();

        let qs: Vec<String> = q.iter().map(|x| format!("{x:.6}")).collect();
        let res = run_params(
            &db,
            &format!(
                "?[id, dist] := ~pts:idx{{ id | query: vec([{}]), k: {K}, ef: 64, \
                 bind_distance: dist }} :order dist :limit {K}",
                qs.join(",")
            ),
            BTreeMap::new(),
        );
        for row in res.rows.iter() {
            if truth.contains(&(row[0].get_int().unwrap() as usize)) {
                hits += 1;
            }
        }
        total += K;
    }
    let recall = hits as f64 / total as f64;
    assert!(
        recall >= 0.9,
        "parallel flat build recall too low: {recall:.3} ({hits}/{total})"
    );
}
