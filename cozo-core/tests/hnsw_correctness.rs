/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use cozo::{DbInstance, NamedRows, ScriptMutability};

fn run(db: &DbInstance, script: &str) -> NamedRows {
    db.run_script(script, Default::default(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("script failed: {e:?}\n--- script ---\n{script}"))
}

#[test]
fn scalar_zero_vectors_are_finite() {
    let db = DbInstance::new("mem", "", "").unwrap();
    let rows = run(
        &db,
        "?[normalized, distance] := \
         normalized = l2_normalize(vec([0.0, 0.0])), \
         distance = cos_dist(vec([0.0, 0.0]), vec([1.0, 0.0]))",
    );
    assert_eq!(rows.rows[0][1].get_float(), Some(2.0));
    assert_eq!(format!("{:?}", rows.rows[0][0]), "vec([0.0, 0.0])");
}

#[test]
fn nulling_a_vector_removes_its_hnsw_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let db = DbInstance::new("sqlite", dir.path().join("null.db").to_str().unwrap(), "").unwrap();
    run(&db, ":create pts { id: Int => emb: <F32; 2>? }");
    run(
        &db,
        "::hnsw create pts:idx { dim: 2, m: 8, dtype: F32, fields: [emb], \
         distance: L2, ef_construction: 32 }",
    );
    run(
        &db,
        "?[id, emb] <- [[1, vec([1.0, 0.0])]] :put pts {id => emb}",
    );
    run(&db, "?[id, emb] <- [[1, null]] :put pts {id => emb}");

    let rows = run(
        &db,
        "node[other] := *pts:idx{fr_id: 1, to_id: other}\n\
         node[other] := *pts:idx{fr_id: other, to_id: 1}\n\
         ?[count(other)] := node[other]",
    );
    assert_eq!(rows.rows[0][0].get_int(), Some(0));

    let search = run(
        &db,
        "?[id] := ~pts:idx{id | query: vec([1.0, 0.0]), k: 10, ef: 32}",
    );
    assert!(search.rows.is_empty());
}

#[test]
fn zero_vectors_produce_finite_cosine_distances_on_both_build_paths() {
    for bulk in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let db = DbInstance::new(
            "sqlite",
            dir.path()
                .join(if bulk { "bulk.db" } else { "inc.db" })
                .to_str()
                .unwrap(),
            "",
        )
        .unwrap();
        run(&db, ":create pts { id: Int => emb: <F32; 2> }");
        if bulk {
            run(
                &db,
                "?[id, emb] <- [[0, vec([0.0, 0.0])], [1, vec([1.0, 0.0])]] \
                 :put pts {id => emb}",
            );
        }
        run(
            &db,
            "::hnsw create pts:idx { dim: 2, m: 8, dtype: F32, fields: [emb], \
             distance: Cosine, ef_construction: 32 }",
        );
        if !bulk {
            run(
                &db,
                "?[id, emb] <- [[0, vec([0.0, 0.0])], [1, vec([1.0, 0.0])]] \
                 :put pts {id => emb}",
            );
        }
        let result = run(
            &db,
            "?[id, dist] := ~pts:idx{id | query: vec([1.0, 0.0]), k: 2, ef: 16, \
             bind_distance: dist} :order id",
        );
        assert_eq!(result.rows.len(), 2);
        for row in result.rows {
            let distance = row[1].get_float().unwrap();
            assert!(distance.is_finite(), "distance must be finite: {distance}");
            if row[0].get_int() == Some(0) {
                assert_eq!(distance, 2.0);
            }
        }
    }
}

#[test]
fn preexisting_nan_vectors_do_not_freeze_l2_or_inner_product_search() {
    for metric in ["L2", "IP"] {
        let dir = tempfile::tempdir().unwrap();
        let db = DbInstance::new(
            "sqlite",
            dir.path().join(format!("{metric}.db")).to_str().unwrap(),
            "",
        )
        .unwrap();
        run(&db, ":create pts { id: Int => emb: <F32; 8> }");
        run(
            &db,
            &format!(
                "::hnsw create pts:idx {{ dim: 8, m: 12, dtype: F32, fields: [emb], \
                 distance: {metric}, ef_construction: 64 }}"
            ),
        );
        // This is the on-disk value older releases produced from l2_normalize(zeros).
        // Several copies make the regression deterministic despite random HNSW levels:
        // at least one poisoned node will be encountered high in the graph.
        let nan_rows = (0..30)
            .map(|id| {
                format!(
                    "[{id}, vec([to_float('NAN'), to_float('NAN'), to_float('NAN'), \
                     to_float('NAN'), to_float('NAN'), to_float('NAN'), to_float('NAN'), \
                     to_float('NAN')])]"
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        run(
            &db,
            &format!("?[id, emb] <- [{nan_rows}] :put pts {{id => emb}}"),
        );
        let mut state = 0xA0761D6478BD642Fu64;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 33) as f64 / (1u64 << 31) as f64) - 1.0
        };
        let vectors: Vec<Vec<f64>> = (0..300)
            .map(|_| {
                let mut vector = (0..8).map(|_| next()).collect::<Vec<_>>();
                let norm = vector.iter().map(|x| x * x).sum::<f64>().sqrt();
                for value in &mut vector {
                    *value = (*value / norm * 1e6).round() / 1e6;
                }
                vector
            })
            .collect();
        let rows = vectors
            .iter()
            .enumerate()
            .map(|(offset, vector)| {
                let values = vector
                    .iter()
                    .map(|value| format!("{value:.6}"))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("[{}, vec([{values}])]", offset + 1)
            })
            .collect::<Vec<_>>()
            .join(",");
        run(
            &db,
            &format!("?[id, emb] <- [{rows}] :put pts {{id => emb}}"),
        );

        let mut hits = 0usize;
        let mut total = 0usize;
        for query_idx in (0..300).step_by(7).take(40) {
            let query = &vectors[query_idx];
            let mut truth = vectors
                .iter()
                .enumerate()
                .map(|(idx, candidate)| {
                    let score = if metric == "L2" {
                        query
                            .iter()
                            .zip(candidate)
                            .map(|(a, b)| (a - b) * (a - b))
                            .sum::<f64>()
                    } else {
                        1.0 - query.iter().zip(candidate).map(|(a, b)| a * b).sum::<f64>()
                    };
                    (idx + 1, score)
                })
                .collect::<Vec<_>>();
            truth.sort_by(|a, b| a.1.total_cmp(&b.1));
            let truth = truth[..10]
                .iter()
                .map(|(id, _)| *id as i64)
                .collect::<std::collections::HashSet<_>>();
            let query_values = query
                .iter()
                .map(|value| format!("{value:.6}"))
                .collect::<Vec<_>>()
                .join(",");
            let result = run(
                &db,
                &format!("?[id] := ~pts:idx{{id | query: vec([{query_values}]), k: 10, ef: 96}}"),
            );
            hits += result
                .rows
                .iter()
                .filter(|row| truth.contains(&row[0].get_int().unwrap()))
                .count();
            total += 10;
        }
        let recall = hits as f64 / total as f64;
        assert!(
            recall >= 0.9,
            "{metric} recall froze around the preexisting NaN vector: {recall:.3} ({hits}/{total})"
        );
    }
}
