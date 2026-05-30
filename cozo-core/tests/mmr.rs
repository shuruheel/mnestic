/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Tests for the mnestic fork's `MaximalMarginalRelevance` fixed rule.

use cozo::{DbInstance, NamedRows, ScriptMutability};
use std::collections::{BTreeMap, HashMap};

fn run(db: &DbInstance, s: &str) -> NamedRows {
    db.run_script(s, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("script failed: {e:?}\n--- script ---\n{s}"))
}

fn rank_map(res: &NamedRows) -> HashMap<String, i64> {
    res.rows
        .iter()
        .map(|r| (r[0].get_str().unwrap().to_string(), r[1].get_int().unwrap()))
        .collect()
}

// Candidates: A and B are near-duplicates (high relevance), C is diverse (lower
// relevance). A=[1,0] rel 1.0; B=[1,0.01] rel 0.9; C=[0,1] rel 0.8.
const CANDS: &str = r#"
    cand[item, rel, v] <- [
        ['A', 1.0, vec([1.0, 0.0])],
        ['B', 0.9, vec([1.0, 0.01])],
        ['C', 0.8, vec([0.0, 1.0])]
    ]
"#;

#[test]
fn mmr_prefers_diversity_over_near_duplicate() {
    let db = DbInstance::default();
    let res = run(
        &db,
        &format!(
            "{CANDS}\n?[item, rank] <~ MaximalMarginalRelevance(cand[item, rel, v], lambda: 0.5)"
        ),
    );
    let m = rank_map(&res);
    // First pick is the most relevant: A.
    assert_eq!(m["A"], 1, "A (highest relevance) selected first");
    // Diversity makes C beat the near-duplicate B despite B's higher relevance.
    assert!(m["C"] < m["B"], "C (diverse) ranked before B (near-dup): {m:?}");
    assert_eq!(m["C"], 2);
    assert_eq!(m["B"], 3);
}

#[test]
fn mmr_lambda_one_is_pure_relevance() {
    let db = DbInstance::default();
    let res = run(
        &db,
        &format!("{CANDS}\n?[item, rank] <~ MMR(cand[item, rel, v], lambda: 1.0)"),
    );
    let m = rank_map(&res);
    // Pure relevance order: A(1.0) > B(0.9) > C(0.8).
    assert_eq!(m["A"], 1);
    assert_eq!(m["B"], 2);
    assert_eq!(m["C"], 3);
}

#[test]
fn mmr_k_limits_selection() {
    let db = DbInstance::default();
    let res = run(
        &db,
        &format!("{CANDS}\n?[item, rank] <~ MMR(cand[item, rel, v], lambda: 0.5, k: 2)"),
    );
    assert_eq!(res.rows.len(), 2, "k=2 selects exactly two");
    let m = rank_map(&res);
    // With lambda 0.5 the top-2 are A then C (diversity).
    assert_eq!(m["A"], 1);
    assert_eq!(m["C"], 2);
    assert!(!m.contains_key("B"));
}
