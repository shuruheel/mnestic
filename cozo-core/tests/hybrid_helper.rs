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

fn ids(res: &NamedRows) -> Vec<String> {
    res.rows
        .iter()
        .map(|r| r[0].get_str().unwrap().to_string())
        .collect()
}

#[test]
fn helper_fuses_hnsw_and_fts() {
    let (db, _dir) = make_db();
    let q = HybridSearch {
        relation: "docs".into(),
        vector_index: "vec".into(),
        query_vector: vec![1.0, 0.0],
        vector_k: 3,
        fts_index: "fts".into(),
        query_text: "cat".into(),
        fts_k: 3,
        ..HybridSearch::default()
    };
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
        .map(|r| (r[0].get_str().unwrap().to_string(), r[1].get_float().unwrap()))
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
    let q = HybridSearch {
        relation: "docs".into(),
        vector_index: "vec".into(),
        query_vector: vec![1.0, 0.0],
        vector_k: 3,
        fts_index: "fts".into(),
        query_text: "cat".into(),
        fts_k: 3,
        ..HybridSearch::default()
    };
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
    let q = HybridSearch {
        relation: "docs".into(),
        vector_index: "vec".into(),
        query_vector: vec![1.0, 0.0],
        vector_k: 4,
        fts_index: "fts".into(),
        query_text: "cat".into(),
        fts_k: 4,
        mmr: Some(MmrParams {
            lambda: 0.7,
            k: 3,
            embedding_col: "emb".into(),
        }),
        ..HybridSearch::default()
    };
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
    let base = HybridSearch {
        relation: "docs".into(),
        vector_index: "vec".into(),
        query_vector: vec![1.0, 0.0],
        vector_k: 2,
        fts_index: "fts".into(),
        query_text: "cat".into(),
        fts_k: 2,
        limit: 10,
        ..HybridSearch::default()
    };
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
    let q = HybridSearch {
        relation: "docs".into(),
        vector_index: "vec".into(),
        query_vector: vec![1.0, 0.0],
        vector_k: 2,
        fts_index: "fts".into(),
        query_text: "zzzznotaword".into(),
        fts_k: 2,
        ..HybridSearch::default()
    };
    let res = db.hybrid_search(&q).unwrap();
    assert!(!res.rows.is_empty(), "semantic leg should still return rows");
    assert_eq!(ids(&res)[0], "d1", "closest embedding still ranks first");
}

#[test]
fn rejects_identifier_injection() {
    let bad = HybridSearch {
        relation: "docs { } ?[x] <- [[1]] :rm docs".into(),
        vector_index: "vec".into(),
        query_vector: vec![1.0, 0.0],
        fts_index: "fts".into(),
        query_text: "cat".into(),
        ..HybridSearch::default()
    };
    assert!(
        build_hybrid_query(&bad).is_err(),
        "an injection-y relation name must be rejected"
    );

    let bad_label = HybridSearch {
        relation: "docs".into(),
        vector_index: "vec".into(),
        query_vector: vec![1.0, 0.0],
        fts_index: "fts".into(),
        query_text: "cat".into(),
        extra_lists: vec![HybridList {
            label: "x'; drop".into(),
            rule_body: "id = 'd1', score = 1.0".into(),
        }],
        ..HybridSearch::default()
    };
    assert!(
        build_hybrid_query(&bad_label).is_err(),
        "an injection-y list label must be rejected"
    );
}

#[test]
fn script_builder_is_inspectable() {
    let q = HybridSearch {
        relation: "docs".into(),
        vector_index: "vec".into(),
        query_vector: vec![1.0, 0.0],
        fts_index: "fts".into(),
        query_text: "cat".into(),
        ..HybridSearch::default()
    };
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
