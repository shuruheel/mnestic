/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A full-text posting whose base row is gone must not fail the search (0.19.4).
//!
//! `fts_search` reads the base row of every ranked hit. A missing row used to
//! raise "corrupted index", so one dangling posting failed every query that
//! ranked it. Found on a production database: releases through 0.12.0 leaked a
//! row's old postings on an in-place `:put` (see `fts_lsh_update_leak.rs`), and
//! a later `:rm` removed only the postings of the row's current text, leaving
//! the leaked ones with no row.
//!
//! The current engine cannot write such postings through `:put` and `:rm`, so
//! these tests make them with bulk import, which does not maintain full-text
//! indexes. Uses the **sqlite** backend (the real stored path).

use cozo::{DbInstance, NamedRows, ScriptMutability, FTS_BASE_ROW_FETCHES};
use std::collections::BTreeMap;

fn run(db: &DbInstance, s: &str) -> NamedRows {
    db.run_script(s, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("script failed: {e:?}\n--- script ---\n{s}"))
}

fn new_db(dir: &tempfile::TempDir) -> DbInstance {
    let db = DbInstance::new(
        "sqlite",
        dir.path().join("fts.db").to_str().unwrap(),
        Default::default(),
    )
    .unwrap();
    run(&db, r":create doc {k: String => body: String}");
    run(
        &db,
        r"::fts create doc:fts { extractor: body, tokenizer: Simple, filters: [Lowercase] }",
    );
    db
}

fn put(db: &DbInstance, rows: &[(&str, &str)]) {
    let rows = rows
        .iter()
        .map(|(k, body)| format!("['{k}', '{body}']"))
        .collect::<Vec<_>>()
        .join(", ");
    run(
        db,
        &format!(r"?[k, body] <- [{rows}] :put doc {{k => body}}"),
    );
}

/// Delete rows through bulk import: the rows go, their postings stay.
fn strand(db: &DbInstance, keys: &[&str]) {
    let rows: Vec<_> = keys.iter().map(|k| serde_json::json!([k])).collect();
    let data = serde_json::json!({ "-doc": { "headers": ["k"], "rows": rows } });
    db.import_relations_str_with_err(&data.to_string()).unwrap();
}

/// Keys for a term query, best first by term frequency.
fn hits(db: &DbInstance, query: &str, k: usize) -> Vec<String> {
    let res = run(
        db,
        &format!(
            r"?[k, s] := ~doc:fts{{k | query: '{query}', k: {k}, score_kind: 'tf', bind_score: s}}
              :order -s, k"
        ),
    );
    res.rows
        .iter()
        .map(|r| r[0].get_str().unwrap().to_string())
        .collect()
}

fn warning_codes(db: &DbInstance) -> Vec<(String, String)> {
    run(db, "::warnings")
        .rows
        .iter()
        .map(|r| {
            (
                r[1].get_str().unwrap().to_string(),
                r[3].get_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// The defect, stated directly. Before the fix this search returned
/// `Err("corrupted index")`.
#[test]
fn search_skips_a_posting_whose_row_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(&dir);
    put(
        &db,
        &[
            ("a", "review of the plan"),
            ("b", "review notes"),
            ("c", "second review"),
        ],
    );
    strand(&db, &["b"]);

    assert_eq!(hits(&db, "review", 10), vec!["a", "c"]);
    // A term only the missing row held: no result, and no error.
    assert!(hits(&db, "notes", 10).is_empty());
}

/// The best-ranked hit is the dangling one. `k: 1` must return the next best,
/// not an empty result: the ranked list is no longer cut to `k` before the
/// base rows are read.
#[test]
fn next_best_hit_takes_the_place_of_a_dangling_one() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(&dir);
    put(
        &db,
        &[
            ("top", "review review review"),
            ("mid", "review review"),
            ("low", "review"),
        ],
    );
    assert_eq!(hits(&db, "review", 1), vec!["top"]);
    strand(&db, &["top"]);

    assert_eq!(hits(&db, "review", 1), vec!["mid"]);
    assert_eq!(hits(&db, "review", 2), vec!["mid", "low"]);
}

/// The shape found in production: the row's text changes without its postings
/// (then: the pre-0.12.1 `:put` leak; here: a bulk import), and a later `:rm`
/// removes only the postings of the current text.
#[test]
fn rm_after_an_unindexed_text_change_leaves_a_posting_that_search_skips() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(&dir);
    put(&db, &[("a", "alpha review"), ("b", "beta review")]);
    let data = serde_json::json!({
        "doc": { "headers": ["k", "body"], "rows": [["a", "alpha"]] }
    });
    db.import_relations_str_with_err(&data.to_string()).unwrap();
    run(&db, r"?[k] <- [['a']] :rm doc {k}");

    assert_eq!(hits(&db, "review", 10), vec!["b"]);
    assert!(hits(&db, "alpha", 10).is_empty());
}

/// A skip is never silent: the process-wide counter moves and the search
/// records a warning whose hint names the repair. `::reindex` then removes the
/// postings, and searches stop warning.
#[test]
fn a_skip_is_counted_and_reported_and_reindex_ends_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(&dir);
    put(&db, &[("a", "review one"), ("b", "review two")]);
    strand(&db, &["b"]);
    run(&db, "::warnings clear");

    // Other tests in this binary may skip hits concurrently, so compare with
    // `>=`; a healthy search never moves the counter.
    let before = cozo::fts_dangling_postings_skipped();
    assert_eq!(hits(&db, "review", 10), vec!["a"]);
    assert!(cozo::fts_dangling_postings_skipped() > before);

    let warnings = warning_codes(&db);
    let (_, hint) = warnings
        .iter()
        .find(|(code, _)| code == "fts.dangling_posting")
        .unwrap_or_else(|| panic!("no fts.dangling_posting warning in {warnings:?}"));
    assert!(hint.contains("::reindex doc"), "{hint}");

    run(&db, "::reindex doc");
    run(&db, "::warnings clear");
    assert_eq!(hits(&db, "review", 10), vec!["a"]);
    assert!(
        warning_codes(&db)
            .iter()
            .all(|(code, _)| code != "fts.dangling_posting"),
        "a rebuilt index must not skip anything"
    );
}

/// Removing the cut to `k` must not add storage reads on a healthy index: an
/// unfiltered `k: 2` search over five matches still reads two base rows.
#[test]
fn a_healthy_index_still_reads_only_k_base_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(&dir);
    put(
        &db,
        &[
            ("a", "review"),
            ("b", "review"),
            ("c", "review"),
            ("d", "review"),
            ("e", "review"),
        ],
    );
    FTS_BASE_ROW_FETCHES.with(|count| count.set(0));
    assert_eq!(hits(&db, "review", 2).len(), 2);
    assert_eq!(FTS_BASE_ROW_FETCHES.with(|count| count.get()), 2);
}
