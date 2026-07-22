/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Behavior tests for the `HybridSearch` **budgeted-expansion mode**
//! (spec `docs/specs/budgeted-traversal.md` §9): a [`GraphLeg`] with
//! `max_nodes` set generates a `BudgetedTraversal` call seeded by the other
//! legs' own top-k, ranked into the fusion by ascending path cost.
//!
//! Gated on `graph-algo`: these tests must *execute* the fixed rule. The
//! builder-level loud error for builds without the feature is guarded in the
//! ungated `hybrid_graph_leg.rs`.
//!
//! Sqlite backend per the fork's testing convention. `HybridSearch`/`GraphLeg`
//! are `#[non_exhaustive]` — construction is `Default` + mutation, as for any
//! downstream user.
#![cfg(feature = "graph-algo")]
#![allow(clippy::field_reassign_with_default)]

use cozo::{DbInstance, GraphLeg, HybridSearch, NamedRows, ScriptMutability};
use std::collections::BTreeMap;

fn run(db: &DbInstance, s: &str) -> NamedRows {
    db.run_script(s, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("script failed: {e:?}\n--- script ---\n{s}"))
}

/// The `hybrid_graph_leg.rs` corpus, with a weighted edge relation:
///
/// ```text
///   d1 ─1.0─▶ d2 ─1.0─▶ d3 ─1.0─▶ d4
///   d1 ─────1.0───────▶ d3
///   d5 ─────1.0─────────────────▶ d4
/// ```
///
/// Only `d1` matches the query (`'alpha'` / `[1,0]`), so with
/// `vector_k = fts_k = 1` the budgeted leg's leg-derived seed set is exactly
/// `{d1}` and everything else surfacing is the expansion's doing.
fn make_db() -> (DbInstance, tempfile::TempDir) {
    make_db_with_edges(&[
        ("d1", "d2", 1.0),
        ("d1", "d3", 1.0),
        ("d2", "d3", 1.0),
        ("d3", "d4", 1.0),
        ("d5", "d4", 1.0),
    ])
}

fn make_db_with_edges(edges: &[(&str, &str, f64)]) -> (DbInstance, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hybrid_budgeted.db");
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
        ":create edges { from: String, to: String => w: Float }",
    );
    let rows: Vec<String> = edges
        .iter()
        .map(|(f, t, w)| format!("['{f}', '{t}', {w:?}]"))
        .collect();
    run(
        &db,
        &format!(
            "?[from, to, w] <- [{}] :put edges {{ from, to => w }}",
            rows.join(", ")
        ),
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

/// Both retrieval legs pinned to `d1` + one budgeted graph leg.
fn budgeted_query(configure: impl FnOnce(&mut GraphLeg)) -> HybridSearch {
    let mut g = GraphLeg::default();
    g.edge_relation = "edges".into();
    g.max_nodes = Some(64);
    g.max_hops = 8;
    configure(&mut g);
    let mut q = HybridSearch::default();
    q.relation = "docs".into();
    q.vector_index = Some("vec".into());
    q.query_vector = vec![1.0, 0.0];
    q.vector_k = 1;
    q.fts_index = Some("fts".into());
    q.query_text = "alpha".into();
    q.fts_k = 1;
    q.graph_legs = vec![g];
    q.limit = 10;
    q
}

/// The graph-leg rows of a detailed result, as `(id, leg_rank)` in row order.
fn graph_rows(res: &NamedRows) -> Vec<(String, f64)> {
    res.rows
        .iter()
        .filter(|r| r[2].get_str() == Some("graph"))
        .map(|r| {
            (
                r[0].get_str().unwrap().to_string(),
                r[3].get_float().unwrap(),
            )
        })
        .collect()
}

/// Leg-seeded expansion surfaces the whole reachable set under a generous
/// budget — and the seed root (depth 0) contributes NO graph-leg row, while
/// the seed still wins overall via the retrieval legs.
#[test]
fn budgeted_leg_expands_from_leg_seeds_and_excludes_roots() {
    let (db, _dir) = make_db();
    let mut q = budgeted_query(|_| {});
    q.detailed = true;
    let res = db.hybrid_search(&q).unwrap();
    let graph: Vec<String> = graph_rows(&res).into_iter().map(|(id, _)| id).collect();
    for id in ["d2", "d3", "d4"] {
        assert!(graph.contains(&id.to_string()), "missing {id}: {graph:?}");
    }
    assert!(
        !graph.contains(&"d1".to_string()),
        "the seed root leaked into the fusion: {graph:?}"
    );
    // d5 is unreachable from d1 in the stored direction.
    assert!(!graph.contains(&"d5".to_string()), "{graph:?}");
}

/// `max_nodes` bites: with a budget of 2 (the root + one admission), exactly
/// one non-seed node survives — the cheapest, ties broken deterministically
/// by node value (d2 < d3 at equal cost 1.0).
#[test]
fn budget_truncates_deterministically() {
    let (db, _dir) = make_db();
    let mut q = budgeted_query(|g| g.max_nodes = Some(2));
    q.detailed = true;
    let res = db.hybrid_search(&q).unwrap();
    let graph: Vec<String> = graph_rows(&res).into_iter().map(|(id, _)| id).collect();
    assert_eq!(graph, vec!["d2".to_string()], "budget 2 = root + d2 only");
}

/// Parity (design §Part I+II tests): generous budget + unit weights +
/// `max_hops` = 2 ⇒ the budgeted leg admits exactly the recursion's candidate
/// set, with matching within-leg ranks.
#[test]
fn parity_with_recursive_mode_on_unit_weights() {
    let (db, _dir) = make_db();
    // Budgeted, weight_col omitted ⇒ every edge costs 1.0.
    let mut budgeted = budgeted_query(|g| g.max_hops = 2);
    budgeted.detailed = true;
    let b = db.hybrid_search(&budgeted).unwrap();

    // The recursion needs explicit seeds; give it the same {d1}.
    let mut recursive = budgeted_query(|g| {
        g.max_nodes = None;
        g.max_hops = 2;
        g.seeds = vec!["d1".into()];
    });
    recursive.detailed = true;
    let r = db.hybrid_search(&recursive).unwrap();

    let b_rows = graph_rows(&b);
    let r_rows = graph_rows(&r);
    assert_eq!(
        b_rows, r_rows,
        "budgeted (unit weights, depth 2) must match the recursion's \
         (id, leg_rank) contributions"
    );
}

/// The f28c496 lesson through the one-call surface: a node reachable *only*
/// through a gated-out node is absent — gated-out nodes neither spend budget
/// nor bridge (d4's only path from d1 runs through d3).
#[test]
fn gated_out_node_is_not_a_bridge() {
    let (db, _dir) = make_db();
    run(&db, ":create live { uid: String => ok: Int }");
    run(
        &db,
        "?[uid, ok] <- [['d1',1],['d2',1],['d3',0],['d4',1],['d5',1]] :put live {uid => ok}",
    );
    let mut q = budgeted_query(|g| {
        g.gate_relation = Some("live".into());
        g.gate_cols = vec!["uid".into(), "ok".into()];
        g.admit = Some("ok == 1".into());
    });
    q.detailed = true;
    let res = db.hybrid_search(&q).unwrap();
    let graph: Vec<String> = graph_rows(&res).into_iter().map(|(id, _)| id).collect();
    assert!(graph.contains(&"d2".to_string()), "{graph:?}");
    assert!(
        !graph.contains(&"d3".to_string()),
        "gated-out d3 must be inadmissible: {graph:?}"
    );
    assert!(
        !graph.contains(&"d4".to_string()),
        "d4 is reachable only through gated-out d3 and must not surface: {graph:?}"
    );
}

/// Permuting edge insertion order changes nothing: the FixedRule guarantees
/// confluence and the builder emits deterministic scripts.
#[test]
fn determinism_under_permuted_edge_insertion() {
    let edges = [
        ("d1", "d2", 1.0),
        ("d1", "d3", 1.0),
        ("d2", "d3", 1.0),
        ("d3", "d4", 1.0),
        ("d5", "d4", 1.0),
    ];
    let mut permuted = edges;
    permuted.reverse();
    permuted.swap(1, 3);
    let (db_a, _da) = make_db_with_edges(&edges);
    let (db_b, _db) = make_db_with_edges(&permuted);
    let mut q = budgeted_query(|g| g.max_nodes = Some(3));
    q.detailed = true;
    let a = db_a.hybrid_search(&q).unwrap();
    let b = db_b.hybrid_search(&q).unwrap();
    assert_eq!(a.rows, b.rows, "insertion order leaked into the output");
}

/// The cached-projection arm (`graph:`) — the production path — returns the
/// same fused result as scanning the edge relation directly.
#[test]
fn projection_arm_matches_edge_relation_arm() {
    let (db, _dir) = make_db();
    run(&db, "::graph create gp {edges: edges}");
    let direct = db.hybrid_search(&budgeted_query(|_| {})).unwrap();
    let via_projection = db
        .hybrid_search(&budgeted_query(|g| {
            g.edge_relation = String::new();
            g.graph = Some("gp".into());
        }))
        .unwrap();
    assert_eq!(direct.rows, via_projection.rows);
}

/// `detailed` with a budgeted leg grows the head by the cheapest-path witness:
/// graph rows carry `(parent, depth)`, other legs' rows carry nulls.
#[test]
fn detailed_head_carries_the_path_witness() {
    let (db, _dir) = make_db();
    let mut q = budgeted_query(|_| {});
    q.detailed = true;
    let res = db.hybrid_search(&q).unwrap();
    assert_eq!(
        res.headers,
        vec![
            "id",
            "score",
            "list_id",
            "leg_rank",
            "leg_score",
            "parent",
            "depth"
        ],
        "witness head"
    );
    for r in &res.rows {
        let is_graph = r[2].get_str() == Some("graph");
        if is_graph {
            assert!(
                r[5].get_str().is_some() && r[6].get_int().is_some(),
                "graph row lacks its witness: {r:?}"
            );
        } else {
            assert!(
                r[5] == cozo::DataValue::Null && r[6] == cozo::DataValue::Null,
                "non-graph row grew a witness: {r:?}"
            );
        }
    }
    // d4's cheapest path from d1 is via d3 at depth 2.
    let d4 = res
        .rows
        .iter()
        .find(|r| r[0].get_str() == Some("d4") && r[2].get_str() == Some("graph"))
        .expect("d4 graph row");
    assert_eq!(d4[5].get_str(), Some("d3"));
    assert_eq!(d4[6].get_int(), Some(2));
}

/// `weight_col` makes cost, not hop count, the ranking signal: with
/// d1→d2 at 5.0 and d1→d3→d4 at 1.0+1.0, the 2-hop d4 (cost 2) must outrank
/// the 1-hop d2 (cost 5) within the graph leg.
#[test]
fn weight_col_ranks_by_path_cost() {
    let (db, _dir) = make_db_with_edges(&[("d1", "d2", 5.0), ("d1", "d3", 1.0), ("d3", "d4", 1.0)]);
    let mut q = budgeted_query(|g| g.weight_col = Some("w".into()));
    q.detailed = true;
    let res = db.hybrid_search(&q).unwrap();
    let graph = graph_rows(&res);
    let rank_of = |id: &str| {
        graph
            .iter()
            .find(|(gid, _)| gid == id)
            .unwrap_or_else(|| panic!("{id} missing from {graph:?}"))
            .1
    };
    assert!(
        rank_of("d3") < rank_of("d2") && rank_of("d4") < rank_of("d2"),
        "cheap 2-hop path must outrank the expensive 1-hop edge: {graph:?}"
    );
}

/// A graph-only budgeted query (no retrieval legs) runs on explicit seeds.
#[test]
fn graph_only_budgeted_runs_on_explicit_seeds() {
    let (db, _dir) = make_db();
    let mut g = GraphLeg::default();
    g.edge_relation = "edges".into();
    g.max_nodes = Some(64);
    g.max_hops = 8;
    g.seeds = vec!["d1".into()];
    let mut q = HybridSearch::default();
    q.relation = "docs".into();
    q.graph_legs = vec![g];
    q.limit = 10;
    let order = ids(&db.hybrid_search(&q).unwrap());
    for id in ["d2", "d3", "d4"] {
        assert!(pos(&order, id).is_some(), "missing {id}: {order:?}");
    }
    assert!(
        pos(&order, "d1").is_none(),
        "the explicit seed is a root and contributes no row: {order:?}"
    );
}
