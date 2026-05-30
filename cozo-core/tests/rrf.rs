/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Tests for the mnestic fork's `ReciprocalRankFusion` fixed rule (hybrid retrieval).

use cozo::{DbInstance, NamedRows, ScriptMutability};
use std::collections::{BTreeMap, HashMap};

fn run(db: &DbInstance, s: &str) -> NamedRows {
    db.run_script(s, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("script failed: {e:?}\n--- script ---\n{s}"))
}

fn fused_map(res: &NamedRows) -> HashMap<String, f64> {
    res.rows
        .iter()
        .map(|r| (r[0].get_str().unwrap().to_string(), r[1].get_float().unwrap()))
        .collect()
}

#[test]
fn rrf_fuses_two_ranked_lists() {
    let db = DbInstance::default();
    // List "a" (desc): x>y>z. List "b" (desc): y>z>w.
    let res = run(
        &db,
        r#"
        combined[lid, item, score] <- [
            ['a', 'x', 0.9], ['a', 'y', 0.8], ['a', 'z', 0.7],
            ['b', 'y', 0.95], ['b', 'z', 0.85], ['b', 'w', 0.5]
        ]
        ?[item, fused] <~ ReciprocalRankFusion(combined[lid, item, score], k: 60)
    "#,
    );
    let m = fused_map(&res);

    // x: rank1 in a only.            => 1/61
    // y: rank2 in a, rank1 in b.     => 1/62 + 1/61
    // z: rank3 in a, rank2 in b.     => 1/63 + 1/62
    // w: rank3 in b only.            => 1/63
    let approx = |a: f64, b: f64| (a - b).abs() < 1e-9;
    assert!(approx(m["x"], 1.0 / 61.0), "x = {}", m["x"]);
    assert!(approx(m["y"], 1.0 / 62.0 + 1.0 / 61.0), "y = {}", m["y"]);
    assert!(approx(m["z"], 1.0 / 63.0 + 1.0 / 62.0), "z = {}", m["z"]);
    assert!(approx(m["w"], 1.0 / 63.0), "w = {}", m["w"]);

    // Fused ranking must be y > z > x > w.
    assert!(m["y"] > m["z"] && m["z"] > m["x"] && m["x"] > m["w"]);
}

#[test]
fn rrf_k_changes_smoothing_and_alias_works() {
    let db = DbInstance::default();
    // Smaller k weights top ranks more heavily. Use the `RRF` alias too.
    let res = run(
        &db,
        r#"
        combined[lid, item, score] <- [['a', 'x', 1.0], ['a', 'y', 0.5]]
        ?[item, fused] <~ RRF(combined[lid, item, score], k: 1)
    "#,
    );
    let m = fused_map(&res);
    // k=1: x rank1 => 1/2 = 0.5 ; y rank2 => 1/3.
    assert!((m["x"] - 0.5).abs() < 1e-9, "x = {}", m["x"]);
    assert!((m["y"] - 1.0 / 3.0).abs() < 1e-9, "y = {}", m["y"]);
}

#[test]
fn rrf_ascending_direction() {
    let db = DbInstance::default();
    // descending: false → lower score ranks first (e.g. raw vector distance).
    let res = run(
        &db,
        r#"
        combined[lid, item, score] <- [['a', 'near', 0.1], ['a', 'far', 0.9]]
        ?[item, fused] <~ ReciprocalRankFusion(combined[lid, item, score], k: 60, descending: false)
    "#,
    );
    let m = fused_map(&res);
    // ascending: near(0.1) is rank1, far(0.9) is rank2.
    assert!(m["near"] > m["far"], "near {} should beat far {}", m["near"], m["far"]);
    assert!((m["near"] - 1.0 / 61.0).abs() < 1e-9);
}

#[test]
fn rrf_default_k_is_60() {
    let db = DbInstance::default();
    let res = run(
        &db,
        r#"
        combined[lid, item, score] <- [['a', 'x', 1.0]]
        ?[item, fused] <~ ReciprocalRankFusion(combined[lid, item, score])
    "#,
    );
    let m = fused_map(&res);
    assert!((m["x"] - 1.0 / 61.0).abs() < 1e-9, "default k=60 => 1/61");
}
