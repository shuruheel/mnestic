/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Tests for the one-call `hybrid_search` helper (mnestic fork addition). Uses
//! the sqlite backend (real stored-relation / index path) per the fork's
//! testing convention.
//!
//! `HybridSearch` is `#[non_exhaustive]`, so struct literals are not
//! constructible outside the defining crate — these tests build queries the
//! way downstream users do: `Default` + field mutation.
#![allow(clippy::field_reassign_with_default)]

use cozo::{
    build_hybrid_query, DbInstance, HybridList, HybridSearch, MmrParams, NamedRows,
    ScriptMutability,
};
use std::collections::BTreeMap;

fn run(db: &DbInstance, s: &str) -> NamedRows {
    db.run_script(s, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("script failed: {e:?}\n--- script ---\n{s}"))
}

/// A tiny corpus with text + 2-D embeddings, plus HNSW and FTS indices.
fn make_db() -> (DbInstance, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hybrid.db");
    let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
    run(
        &db,
        r#"
        ?[id, text, emb] <- [
            ['d1', 'the cat sat on the mat', [1.0, 0.0]],
            ['d2', 'a dog ran in the park',  [0.9, 0.1]],
            ['d3', 'cats and dogs are pets', [0.0, 1.0]],
            ['d4', 'the weather is nice',    [0.1, 0.9]]
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
            extractor: text, tokenizer: Simple,
            filters: [Lowercase, Stemmer('English'), Stopwords('en')]
        }"#,
    );
    (db, dir)
}

/// The canonical both-legs query against `make_db`'s corpus.
fn base_query() -> HybridSearch {
    let mut q = HybridSearch::default();
    q.relation = "docs".into();
    q.vector_index = Some("vec".into());
    q.query_vector = vec![1.0, 0.0];
    q.fts_index = Some("fts".into());
    q.query_text = "cat".into();
    q
}

fn ids(res: &NamedRows) -> Vec<String> {
    res.rows
        .iter()
        .map(|r| r[0].get_str().unwrap().to_string())
        .collect()
}

#[test]
fn helper_fuses_hnsw_and_fts() {
    let (db, _dir) = make_db();
    let mut q = base_query();
    q.vector_k = 3;
    q.fts_k = 3;
    let res = db
        .hybrid_search(&q)
        .unwrap_or_else(|e| panic!("hybrid_search failed: {e:?}"));
    let order = ids(&res);
    assert!(!order.is_empty(), "fusion produced no rows");
    // d1 is rank-1 in BOTH legs (closest to [1,0] AND best "cat" match).
    assert_eq!(order[0], "d1", "d1 should rank first; order = {order:?}");

    // d1's fused score must exceed every single-leg item.
    let fused: std::collections::HashMap<String, f64> = res
        .rows
        .iter()
        .map(|r| {
            (
                r[0].get_str().unwrap().to_string(),
                r[1].get_float().unwrap(),
            )
        })
        .collect();
    for (id, score) in &fused {
        if id != "d1" {
            assert!(fused["d1"] > *score, "d1 should outscore {id}");
        }
    }
}

#[test]
fn helper_matches_handwritten_pattern() {
    let (db, _dir) = make_db();
    let mut q = base_query();
    q.vector_k = 3;
    q.fts_k = 3;
    let helper = ids(&db.hybrid_search(&q).unwrap());

    // The same pattern, written by hand (mirrors tests/hybrid_retrieval_e2e.rs).
    let handwritten = run(
        &db,
        r#"
        sem[id, score] := ~docs:vec{ id | query: q, k: 3, ef: 50, bind_distance: dist },
                          q = vec([1.0, 0.0]), score = -dist
        txt[id, score] := ~docs:fts{ id | query: 'cat', k: 3, bind_score: score }
        combined[lid, id, score] := sem[id, score], lid = 'semantic'
        combined[lid, id, score] := txt[id, score], lid = 'text'
        ?[id, fused] <~ ReciprocalRankFusion(combined[lid, id, score], k: 60)
        :order -fused
    "#,
    );
    assert_eq!(
        helper,
        ids(&handwritten),
        "helper output should match the hand-written pattern"
    );
}

#[test]
fn helper_with_mmr_rerank() {
    let (db, _dir) = make_db();
    let mut q = base_query();
    q.vector_k = 4;
    q.fts_k = 4;
    q.mmr = Some(MmrParams {
        lambda: 0.7,
        k: 3,
        embedding_col: "emb".into(),
    });
    let res = db.hybrid_search(&q).unwrap();
    assert_eq!(res.rows.len(), 3, "MMR k=3 should yield three results");
    assert_eq!(
        res.rows[0][0].get_str(),
        Some("d1"),
        "MMR keeps the most-relevant d1 first"
    );
}

#[test]
fn helper_extra_traversal_list_is_fused() {
    let (db, _dir) = make_db();
    // An extra "related" signal that strongly ranks d4 (which neither the
    // semantic nor keyword leg favors). Folding it in should surface d4.
    let mut base = base_query();
    base.vector_k = 2;
    base.fts_k = 2;
    base.limit = 10;
    let without = ids(&db.hybrid_search(&base).unwrap());
    assert!(
        !without.contains(&"d4".to_string()),
        "baseline should not surface d4; got {without:?}"
    );

    let mut with_extra = base.clone();
    with_extra.extra_lists = vec![HybridList {
        label: "related".into(),
        rule_body: "id = 'd4', score = 1.0".into(),
    }];
    let with = ids(&db.hybrid_search(&with_extra).unwrap());
    assert!(
        with.contains(&"d4".to_string()),
        "extra list should surface d4; got {with:?}"
    );
}

#[test]
fn helper_handles_empty_fts_leg() {
    let (db, _dir) = make_db();
    // A keyword with no matches — the FTS leg is empty, the semantic leg carries.
    let mut q = base_query();
    q.vector_k = 2;
    q.query_text = "zzzznotaword".into();
    q.fts_k = 2;
    let res = db.hybrid_search(&q).unwrap();
    assert!(
        !res.rows.is_empty(),
        "semantic leg should still return rows"
    );
    assert_eq!(ids(&res)[0], "d1", "closest embedding still ranks first");
}

#[test]
fn rejects_identifier_injection() {
    let mut bad = base_query();
    bad.relation = "docs { } ?[x] <- [[1]] :rm docs".into();
    assert!(
        build_hybrid_query(&bad).is_err(),
        "an injection-y relation name must be rejected"
    );

    let mut bad_label = base_query();
    bad_label.extra_lists = vec![HybridList {
        label: "x'; drop".into(),
        rule_body: "id = 'd1', score = 1.0".into(),
    }];
    assert!(
        build_hybrid_query(&bad_label).is_err(),
        "an injection-y list label must be rejected"
    );
}

#[test]
fn script_builder_is_inspectable() {
    let q = base_query();
    let (script, params) = build_hybrid_query(&q).unwrap();
    // Values are params, not interpolated.
    assert!(params.contains_key("qv"));
    assert!(params.contains_key("qt"));
    assert!(script.contains("query: vec($qv)"));
    assert!(script.contains("query: $qt"));
    assert!(script.contains("ReciprocalRankFusion"));
    // No MMR requested → no rerank stage.
    assert!(!script.contains("MaximalMarginalRelevance"));
}

// Optional legs (0.13.0): single-leg configurations run and fuse end-to-end.
#[test]
fn helper_vector_only_runs() {
    let (db, _dir) = make_db();
    let mut q = HybridSearch::default();
    q.relation = "docs".into();
    q.vector_index = Some("vec".into());
    q.query_vector = vec![1.0, 0.0];
    q.vector_k = 3;
    let res = db.hybrid_search(&q).unwrap();
    assert_eq!(ids(&res)[0], "d1", "closest embedding ranks first");
}

#[test]
fn helper_fts_only_runs() {
    let (db, _dir) = make_db();
    let mut q = HybridSearch::default();
    q.relation = "docs".into();
    q.fts_index = Some("fts".into());
    q.query_text = "cat".into();
    q.fts_k = 3;
    let res = db.hybrid_search(&q).unwrap();
    let order = ids(&res);
    assert!(!order.is_empty(), "FTS-only fusion produced no rows");
    assert!(
        order.iter().all(|id| id == "d1" || id == "d3"),
        "only the 'cat' matches belong here; got {order:?}"
    );
}

#[test]
fn helper_zero_legs_is_a_loud_error() {
    let mut q = HybridSearch::default();
    q.relation = "docs".into();
    let e = format!("{:?}", build_hybrid_query(&q).unwrap_err());
    assert!(e.contains("at least one leg"), "{e}");
}

#[test]
fn helper_detailed_returns_per_leg_contributions() {
    let (db, _dir) = make_db();
    let mut q = base_query();
    q.vector_k = 3;
    q.fts_k = 3;
    q.detailed = true;
    let res = db
        .hybrid_search(&q)
        .unwrap_or_else(|e| panic!("hybrid_search failed: {e:?}"));
    assert_eq!(
        res.headers,
        vec!["id", "score", "list_id", "leg_rank", "leg_score"],
        "long-format head"
    );
    assert!(!res.rows.is_empty(), "fusion produced no rows");

    // d1 matches both legs ('cat' text + nearest embedding) => two rows, one
    // per leg, same fused score, leg_rank 1 in each.
    let d1_rows: Vec<_> = res
        .rows
        .iter()
        .filter(|r| r[0].get_str() == Some("d1"))
        .collect();
    assert_eq!(
        d1_rows.len(),
        2,
        "d1 contributes from both legs: {:?}",
        res.rows
    );
    let lists: Vec<&str> = d1_rows.iter().map(|r| r[2].get_str().unwrap()).collect();
    assert!(
        lists.contains(&"semantic") && lists.contains(&"text"),
        "lists = {lists:?}"
    );
    assert_eq!(
        d1_rows[0][1], d1_rows[1][1],
        "fused score repeats across an item's rows"
    );
    for r in &d1_rows {
        assert_eq!(r[3].get_float(), Some(1.0), "d1 is rank 1 in both legs");
    }

    // Single-leg items get exactly one row: d2 ('a dog ran in the park') is in
    // the semantic top-3 but never matches 'cat'; d3 ('cats and dogs are pets')
    // matches the text leg but is the farthest embedding (outside vector_k=3).
    let leg_of = |id: &str| -> Vec<&str> {
        res.rows
            .iter()
            .filter(|r| r[0].get_str() == Some(id))
            .map(|r| r[2].get_str().unwrap())
            .collect()
    };
    assert_eq!(leg_of("d2"), vec!["semantic"], "rows = {:?}", res.rows);
    assert_eq!(leg_of("d3"), vec!["text"], "rows = {:?}", res.rows);
}

#[test]
fn helper_detailed_with_mmr_joins_detail_onto_selection() {
    let (db, _dir) = make_db();
    let mut q = base_query();
    q.vector_k = 4;
    q.fts_k = 4;
    q.detailed = true;
    q.mmr = Some(MmrParams {
        lambda: 0.5,
        k: 2,
        embedding_col: "emb".into(),
    });
    let res = db
        .hybrid_search(&q)
        .unwrap_or_else(|e| panic!("hybrid_search failed: {e:?}"));
    assert_eq!(
        res.headers,
        vec!["id", "rank", "score", "list_id", "leg_rank", "leg_score"],
        "MMR detailed head"
    );
    // MMR selected 2 items; rows expand per contributing leg, so distinct ids
    // must be exactly 2.
    let mut ids: Vec<&str> = res.rows.iter().map(|r| r[0].get_str().unwrap()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(
        ids.len(),
        2,
        "MMR k=2 bounds the items; rows = {:?}",
        res.rows
    );
}

#[test]
fn detailed_script_widens_limit_by_leg_count() {
    let mut q = base_query();
    q.detailed = true;
    q.limit = 10;
    let (script, _params) = build_hybrid_query(&q).unwrap();
    assert!(script.contains("detailed: true"), "script: {script}");
    assert!(
        script.contains(":limit 20"),
        "limit 10 × 2 legs = 20; script: {script}"
    );
}
