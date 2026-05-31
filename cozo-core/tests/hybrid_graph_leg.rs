/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Tests for the mnestic fork's **native 3-way fused recall** — a typed
//! [`GraphLeg`] folded into `hybrid_search` alongside the vector (HNSW) and
//! keyword (FTS) legs (DEVELOPMENT.md Bet 1a).
//!
//! The graph leg generates a *recursive bounded shortest-path* rule that a
//! single spliced [`cozo::HybridList`] body cannot express, scores reached
//! nodes by their minimum hop distance, and contributes that ranked list to the
//! same Reciprocal Rank Fusion — in one call, no second `run_script`.
//!
//! Uses the **sqlite** backend (real stored-relation / index path) per the
//! fork's testing convention.

use cozo::{build_hybrid_query, DbInstance, GraphLeg, HybridSearch, NamedRows, ScriptMutability};
use std::collections::BTreeMap;

fn run(db: &DbInstance, s: &str) -> NamedRows {
    db.run_script(s, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("script failed: {e:?}\n--- script ---\n{s}"))
}

/// A tiny corpus + a directed edge relation:
///
/// ```text
///   d1 ──▶ d2 ──▶ d3 ──▶ d4
///   d1 ─────────▶ d3
///   d5 ─────────────────▶ d4
/// ```
///
/// Only `d1` matches the query (`'alpha'` / `[1,0]`); `d2..d5` are far on both
/// the vector and keyword legs, so anything else surfacing is the graph leg.
fn make_db() -> (DbInstance, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hybrid_graph.db");
    let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
    run(
        &db,
        r#"
        ?[id, text, emb] <- [
            ['d1', 'alpha',   [1.0, 0.0]],
            ['d2', 'beta',    [0.0, 1.0]],
            ['d3', 'gamma',   [0.0, 1.0]],
            ['d4', 'delta',   [0.0, 1.0]],
            ['d5', 'epsilon', [-1.0, 0.0]]
        ]
        :create docs { id: String => text: String, emb: <F32; 2> }
    "#,
    );
    run(
        &db,
        r#"::hnsw create docs:vec {
            dim: 2, m: 50, dtype: F32, fields: [emb],
            distance: L2, ef_construction: 20
        }"#,
    );
    run(
        &db,
        r#"::fts create docs:fts {
            extractor: text, tokenizer: Simple, filters: [Lowercase]
        }"#,
    );
    run(
        &db,
        r#"
        ?[from, to] <- [
            ['d1', 'd2'], ['d1', 'd3'], ['d2', 'd3'], ['d3', 'd4'], ['d5', 'd4']
        ]
        :create edges { from: String, to: String }
    "#,
    );
    (db, dir)
}

fn ids(res: &NamedRows) -> Vec<String> {
    res.rows
        .iter()
        .map(|r| r[0].get_str().unwrap().to_string())
        .collect()
}

fn pos(order: &[String], id: &str) -> Option<usize> {
    order.iter().position(|x| x == id)
}

/// A baseline query that only `d1` can satisfy on the vector + keyword legs
/// (`vector_k`/`fts_k` = 1 ⇒ each fixed leg returns just `d1`), so the fused
/// output beyond `d1` is entirely the graph leg's doing.
fn graph_dominated(seeds: &[&str], max_hops: usize, undirected: bool) -> HybridSearch {
    HybridSearch {
        relation: "docs".into(),
        vector_index: "vec".into(),
        query_vector: vec![1.0, 0.0],
        vector_k: 1,
        fts_index: "fts".into(),
        query_text: "alpha".into(),
        fts_k: 1,
        graph_legs: vec![GraphLeg {
            edge_relation: "edges".into(),
            seeds: seeds.iter().map(|s| (*s).into()).collect(),
            max_hops,
            undirected,
            ..GraphLeg::default()
        }],
        limit: 10,
        ..HybridSearch::default()
    }
}

/// The graph leg must surface a neighbour (`d4`, 2 hops from `d1`) that is poor
/// on *both* the vector and keyword legs — i.e. recall the fixed legs miss.
#[test]
fn graph_leg_surfaces_unmatched_neighbor() {
    let (db, _dir) = make_db();

    // Baseline: vector + FTS only. d4 should be nowhere near (it is far on both).
    let mut baseline = graph_dominated(&["d1"], 2, false);
    baseline.graph_legs.clear();
    let base_ids = ids(&db.hybrid_search(&baseline).unwrap());
    assert!(
        pos(&base_ids, "d4").is_none(),
        "precondition: d4 should not surface without the graph leg; got {base_ids:?}"
    );

    // With a 2-hop graph leg from d1, d4 (d1→d3→d4) is recalled.
    let q = graph_dominated(&["d1"], 2, false);
    let order = ids(&db.hybrid_search(&q).unwrap());
    assert!(
        pos(&order, "d4").is_some(),
        "graph leg should recall d4 within 2 hops; got {order:?}"
    );
    // The seed itself is the query anchor and is not scored by the graph leg,
    // but it still wins via the vector + keyword legs.
    assert_eq!(order.first().map(String::as_str), Some("d1"), "order = {order:?}");
}

/// Closer nodes outrank farther ones: with the graph leg dominating, a 1-hop
/// neighbour (`d2`) must rank above a 2-hop one (`d4`). Exercises the
/// `min(dist)` distance scoring end-to-end.
#[test]
fn graph_leg_closer_outranks_farther() {
    let (db, _dir) = make_db();
    let order = ids(&db.hybrid_search(&graph_dominated(&["d1"], 2, false)).unwrap());
    let p2 = pos(&order, "d2").expect("d2 (1 hop) should be present");
    let p4 = pos(&order, "d4").expect("d4 (2 hops) should be present");
    assert!(
        p2 < p4,
        "1-hop d2 (pos {p2}) must outrank 2-hop d4 (pos {p4}); order = {order:?}"
    );
}

/// `max_hops` bounds the recursion: at 1 hop, the 2-hop node `d4` must NOT be
/// recalled (and nothing else pulls it in, by construction).
#[test]
fn graph_leg_respects_hop_bound() {
    let (db, _dir) = make_db();
    let order = ids(&db.hybrid_search(&graph_dominated(&["d1"], 1, false)).unwrap());
    assert!(
        pos(&order, "d2").is_some() && pos(&order, "d3").is_some(),
        "1-hop neighbours d2, d3 should be present; got {order:?}"
    );
    assert!(
        pos(&order, "d4").is_none(),
        "d4 is 2 hops away and must be excluded at max_hops=1; got {order:?}"
    );
}

/// Undirected traversal also follows edges in reverse: from `d3`, a 1-hop
/// undirected expansion reaches `d4` (forward `d3→d4`) *and* `d2` (reverse of
/// `d2→d3`); directed reaches only `d4`. (We probe `d2` rather than `d1`: `d1`
/// is the query anchor and is always returned by the fixed legs, so it can't
/// distinguish the two directions.)
#[test]
fn graph_leg_undirected_follows_reverse_edges() {
    let (db, _dir) = make_db();

    let directed = ids(&db.hybrid_search(&graph_dominated(&["d3"], 1, false)).unwrap());
    assert!(
        pos(&directed, "d2").is_none(),
        "directed 1-hop from d3 must not reach d2 (edge is d2→d3); got {directed:?}"
    );

    let undirected = ids(&db.hybrid_search(&graph_dominated(&["d3"], 1, true)).unwrap());
    assert!(
        pos(&undirected, "d2").is_some(),
        "undirected 1-hop from d3 should reach d2 via the reverse edge; got {undirected:?}"
    );
}

/// Multiple seeds union their reachable sets: from `{d1, d5}` at 1 hop we get
/// `d2`/`d3` (from d1) and `d4` (from d5).
#[test]
fn graph_leg_multiple_seeds_union() {
    let (db, _dir) = make_db();
    let order = ids(&db.hybrid_search(&graph_dominated(&["d1", "d5"], 1, false)).unwrap());
    for id in ["d2", "d3", "d4"] {
        assert!(
            pos(&order, id).is_some(),
            "multi-seed 1-hop should recall {id}; got {order:?}"
        );
    }
}

/// The generated script must contain the recursive shortest-path rule (with
/// `min(...)` aggregation and the hop-bound guard), carry seeds as params (not
/// interpolated), and tag the fusion list with the leg label.
#[test]
fn graph_leg_script_is_recursive_and_parametrised() {
    let q = HybridSearch {
        relation: "docs".into(),
        vector_index: "vec".into(),
        query_vector: vec![1.0, 0.0],
        fts_index: "fts".into(),
        query_text: "alpha".into(),
        graph_legs: vec![GraphLeg {
            label: "graph".into(),
            edge_relation: "edges".into(),
            seeds: vec!["d1".into(), "d2".into()],
            max_hops: 3,
            ..GraphLeg::default()
        }],
        ..HybridSearch::default()
    };
    let (script, params) = build_hybrid_query(&q).unwrap();

    assert!(script.contains("hg0_reach[__to, min(__d)]"), "no recursive min-rule:\n{script}");
    assert!(script.contains("__pd < 3.0"), "no hop-bound guard:\n{script}");
    assert!(script.contains("__lid = 'graph'"), "graph leg not fused:\n{script}");
    // Seeds are params, never string-interpolated into the script body.
    assert!(script.contains("$hg0_seed0") && script.contains("$hg0_seed1"), "seeds not params:\n{script}");
    assert!(!script.contains("'d1'"), "seed value leaked into script text:\n{script}");
    assert_eq!(params.get("hg0_seed0").and_then(|v| v.get_str()), Some("d1"));
    assert_eq!(params.get("hg0_seed1").and_then(|v| v.get_str()), Some("d2"));
}

/// Validation rejects an empty seed set and a zero hop bound.
#[test]
fn graph_leg_validates_inputs() {
    let base = HybridSearch {
        relation: "docs".into(),
        vector_index: "vec".into(),
        query_vector: vec![1.0, 0.0],
        fts_index: "fts".into(),
        query_text: "alpha".into(),
        ..HybridSearch::default()
    };

    let mut empty_seeds = base.clone();
    empty_seeds.graph_legs = vec![GraphLeg {
        edge_relation: "edges".into(),
        seeds: vec![],
        ..GraphLeg::default()
    }];
    assert!(build_hybrid_query(&empty_seeds).is_err(), "empty seeds must be rejected");

    let mut zero_hops = base;
    zero_hops.graph_legs = vec![GraphLeg {
        edge_relation: "edges".into(),
        seeds: vec!["d1".into()],
        max_hops: 0,
        ..GraphLeg::default()
    }];
    assert!(build_hybrid_query(&zero_hops).is_err(), "max_hops=0 must be rejected");
}
