/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Candidate-aware FTS acceptance tests.
//!
//! `candidates:` restricts eligibility before scoring/top-k collection without
//! changing corpus-global BM25 statistics. These use sqlite so the stored path,
//! posting decoding, and base-row fetches are all exercised.

use cozo::{DataValue, DbInstance, NamedRows, ScriptMutability, FTS_BASE_ROW_FETCHES};
use std::collections::{BTreeMap, HashMap};

fn db() -> DbInstance {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fts-candidates.db");
    std::mem::forget(dir);
    DbInstance::new("sqlite", path.to_str().unwrap(), Default::default()).unwrap()
}

fn run_with(db: &DbInstance, script: &str, params: BTreeMap<String, DataValue>) -> NamedRows {
    db.run_script(script, params, ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("script failed: {e:?}\n--- script ---\n{script}"))
}

fn run(db: &DbInstance, script: &str) -> NamedRows {
    run_with(db, script, BTreeMap::new())
}

fn setup(db: &DbInstance) {
    run(db, r":create doc {k: String => body: String}");
    run(
        db,
        r"::fts create doc:fts { extractor: body, tokenizer: Simple, filters: [Lowercase] }",
    );
}

fn put(db: &DbInstance, rows: &str) {
    run(
        db,
        &format!(r"?[k, body] <- [{rows}] :put doc {{k => body}}"),
    );
}

fn candidate_params(values: Vec<DataValue>) -> BTreeMap<String, DataValue> {
    BTreeMap::from([("allow".to_string(), DataValue::List(values))])
}

#[test]
fn candidate_scope_is_applied_before_top_k() {
    let db = db();
    setup(&db);
    let mut rows = vec!["['target', 'needle']".to_string()];
    rows.extend((0..50).map(|i| format!("['foreign-{i}', 'needle needle needle needle']")));
    put(&db, &rows.join(","));

    let global = run(&db, r"?[k] := ~doc:fts{k | query: 'needle', k: 1} :order k");
    assert_ne!(global.rows[0][0].get_str(), Some("target"));

    let scoped = run_with(
        &db,
        r"?[k] := ~doc:fts{k | query: 'needle', k: 1, candidates: $allow}",
        candidate_params(vec![DataValue::from("target")]),
    );
    assert_eq!(scoped.rows.len(), 1);
    assert_eq!(scoped.rows[0][0].get_str(), Some("target"));

    // `filter:` disables the ordinary pre-fetch top-k truncation, making the
    // base-row fetch count expose how much post-ranking storage work remains.
    // The candidate-aware path must fetch only the eligible row, not all 51
    // globally matching rows.
    FTS_BASE_ROW_FETCHES.with(|count| count.set(0));
    let unbounded_fetches = run(
        &db,
        r"?[k] := ~doc:fts{k | query: 'needle', k: 1, filter: k == 'target'}",
    );
    assert_eq!(unbounded_fetches.rows.len(), 1);
    assert_eq!(FTS_BASE_ROW_FETCHES.with(|count| count.get()), 51);

    FTS_BASE_ROW_FETCHES.with(|count| count.set(0));
    let bounded_fetches = run_with(
        &db,
        r"?[k] := ~doc:fts{k | query: 'needle', k: 1,
                                  candidates: $allow, filter: k == 'target'}",
        candidate_params(vec![DataValue::from("target")]),
    );
    assert_eq!(bounded_fetches.rows.len(), 1);
    assert_eq!(FTS_BASE_ROW_FETCHES.with(|count| count.get()), 1);
}

#[test]
fn candidate_scope_handles_boolean_and_proximity_queries() {
    let db = db();
    setup(&db);
    put(
        &db,
        r"
        ['keep', 'alpha beta gamma'],
        ['other', 'alpha beta gamma'],
        ['alpha-only', 'alpha'],
        ['gamma-only', 'gamma']
        ",
    );
    let allow = || candidate_params(vec![DataValue::from("keep")]);
    for query in [
        "alpha AND beta",
        "alpha OR gamma",
        "alpha NOT delta",
        "NEAR/2(alpha beta)",
    ] {
        let result = run_with(
            &db,
            &format!(r"?[k] := ~doc:fts{{k | query: '{query}', k: 10, candidates: $allow}}"),
            allow(),
        );
        assert_eq!(result.rows.len(), 1, "query {query:?}: {result:?}");
        assert_eq!(result.rows[0][0].get_str(), Some("keep"));
    }
}

#[test]
fn candidate_scope_preserves_scores_and_composes_with_filter() {
    let db = db();
    setup(&db);
    put(
        &db,
        r"
        ['keep', 'rare common'],
        ['blocked', 'rare common common common'],
        ['common-1', 'common'],
        ['common-2', 'common'],
        ['common-3', 'common']
        ",
    );
    let unrestricted = run(
        &db,
        r"?[k, s] := ~doc:fts{k, body | query: 'rare OR common', k: 20, bind_score: s}",
    );
    let unrestricted: HashMap<_, _> = unrestricted
        .rows
        .into_iter()
        .map(|row| {
            (
                row[0].get_str().unwrap().to_string(),
                row[1].get_float().unwrap(),
            )
        })
        .collect();

    let scoped = run_with(
        &db,
        r"?[k, s] := ~doc:fts{k, body | query: 'rare OR common', k: 20,
                                      bind_score: s, candidates: $allow,
                                      filter: k != 'blocked'}",
        candidate_params(vec![DataValue::from("keep"), DataValue::from("blocked")]),
    );
    assert_eq!(scoped.rows.len(), 1);
    assert_eq!(scoped.rows[0][0].get_str(), Some("keep"));
    let scoped_score = scoped.rows[0][1].get_float().unwrap();
    assert!(
        (scoped_score - unrestricted["keep"]).abs() < 1e-12,
        "candidate restriction changed BM25 score: {scoped_score} vs {}",
        unrestricted["keep"]
    );
}

#[test]
fn candidate_keys_are_arity_checked_and_type_coerced() {
    let db = db();
    run(&db, r":create note {ns: String, seq: Int => body: String}");
    run(
        &db,
        r"::fts create note:fts { extractor: body, tokenizer: Simple, filters: [Lowercase] }",
    );
    run(
        &db,
        r"?[ns, seq, body] <- [['a', 1, 'needle'], ['a', 2, 'needle']] :put note {ns, seq => body}",
    );
    let composite = run_with(
        &db,
        r"?[ns, seq] := ~note:fts{ns, seq | query: 'needle', k: 10, candidates: $allow}",
        candidate_params(vec![DataValue::List(vec![
            DataValue::from("a"),
            DataValue::from(2),
        ])]),
    );
    assert_eq!(composite.rows.len(), 1);
    assert_eq!(composite.rows[0][1].get_int(), Some(2));

    run(&db, r":create floated {k: Float => body: String}");
    run(
        &db,
        r"::fts create floated:fts { extractor: body, tokenizer: Simple, filters: [Lowercase] }",
    );
    run(
        &db,
        r"?[k, body] <- [[1.0, 'needle']] :put floated {k => body}",
    );
    let coerced = run_with(
        &db,
        r"?[k] := ~floated:fts{k | query: 'needle', k: 10, candidates: $allow}",
        candidate_params(vec![DataValue::from(1)]),
    );
    assert_eq!(coerced.rows.len(), 1);
    assert_eq!(coerced.rows[0][0].get_float(), Some(1.0));
}

#[test]
fn candidate_errors_empty_scope_and_multi_parent_are_explicit() {
    let db = db();
    setup(&db);
    put(&db, r"['a', 'alpha'], ['b', 'beta']");

    let empty = run_with(
        &db,
        r"?[k] := ~doc:fts{k | query: 'alpha', k: 10, candidates: $allow}",
        candidate_params(Vec::new()),
    );
    assert!(empty.rows.is_empty());

    let not_a_list = db
        .run_script(
            r"?[k] := ~doc:fts{k | query: 'alpha', k: 10, candidates: 'a'}",
            BTreeMap::new(),
            ScriptMutability::Mutable,
        )
        .unwrap_err();
    assert!(format!("{not_a_list:?}").contains("candidates"));

    let non_constant = db
        .run_script(
            r"?[q, k] := q = 'alpha', ~doc:fts{k | query: q, k: 10, candidates: q}",
            BTreeMap::new(),
            ScriptMutability::Mutable,
        )
        .unwrap_err();
    assert!(format!("{non_constant:?}").contains("constant"));

    let driven = run_with(
        &db,
        r"parent[q] <- [['alpha'], ['beta']]
          ?[q, k] := parent[q], ~doc:fts{k | query: q, k: 10, candidates: $allow}
          :order q, k",
        candidate_params(vec![DataValue::from("a")]),
    );
    assert_eq!(driven.rows.len(), 1);
    assert_eq!(driven.rows[0][0].get_str(), Some("alpha"));
    assert_eq!(driven.rows[0][1].get_str(), Some("a"));
}
