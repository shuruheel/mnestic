/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! End-to-end hybrid retrieval: a real HNSW (vector) search and a real FTS
//! (keyword) search over the same relation, fused with `ReciprocalRankFusion`.
//! This proves the fusion operator on the actual retrieval modalities — not just
//! synthetic ranked lists — which is the core agentic-memory read path.

use cozo::{DbInstance, NamedRows, ScriptMutability};
use std::collections::{BTreeMap, HashMap};

fn run(db: &DbInstance, s: &str) -> NamedRows {
    db.run_script(s, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("script failed: {e:?}\n--- script ---\n{s}"))
}

#[test]
fn hnsw_plus_fts_fused_with_rrf() {
    let db = DbInstance::new("mem", "", "").unwrap();

    // A tiny corpus: text + a 2-D "embedding".
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

    // Hybrid query: semantic search near d1's embedding + keyword search for
    // "cat". Normalise both so higher = better (negate the HNSW distance), union
    // into one [list_id, item, score] relation, and fuse.
    let res = run(
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

    assert!(!res.rows.is_empty(), "fusion produced no rows");
    let order: Vec<String> = res
        .rows
        .iter()
        .map(|r| r[0].get_str().unwrap().to_string())
        .collect();
    let fused: HashMap<String, f64> = res
        .rows
        .iter()
        .map(|r| (r[0].get_str().unwrap().to_string(), r[1].get_float().unwrap()))
        .collect();

    // d1 is rank-1 in BOTH lists (closest embedding to [1,0] AND best "cat"
    // match), so it must be the top fused result.
    assert_eq!(order[0], "d1", "d1 should rank first; order = {order:?}");
    // d1 appears in two lists, so its fused score must exceed any single-list item.
    for (id, score) in &fused {
        if id != "d1" {
            assert!(
                fused["d1"] > *score,
                "d1 ({}) should outscore {id} ({score})",
                fused["d1"]
            );
        }
    }

    // The fused output composes with MMR for a diversity-aware final ordering.
    let res2 = run(
        &db,
        r#"
        sem[id, score] := ~docs:vec{ id | query: q, k: 4, ef: 50, bind_distance: dist },
                          q = vec([1.0, 0.0]), score = -dist
        txt[id, score] := ~docs:fts{ id | query: 'cat', k: 4, bind_score: score }
        combined[lid, id, score] := sem[id, score], lid = 'semantic'
        combined[lid, id, score] := txt[id, score], lid = 'text'
        fused[id, score] <~ ReciprocalRankFusion(combined[lid, id, score], k: 60)
        cand[id, score, emb] := fused[id, score], *docs{ id, emb }
        ?[id, rank] <~ MaximalMarginalRelevance(cand[id, score, emb], lambda: 0.7, k: 3)
        :order rank
    "#,
    );
    assert_eq!(res2.rows.len(), 3, "MMR k=3 should yield three results");
    assert_eq!(
        res2.rows[0][0].get_str(),
        Some("d1"),
        "MMR keeps the most-relevant d1 first"
    );
}
