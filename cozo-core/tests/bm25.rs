/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Tests for the mnestic fork's BM25-correct FTS scoring (DEVELOPMENT.md Bet 1b).
//!
//! Two defects in upstream's `tf_idf` scoring that drag fused recall (bench: FTS
//! agreement 0.72 vs vector 0.99 / graph 1.00):
//!   A. scoring is raw `tf * idf` — no Okapi `k1` saturation, no `b` length
//!      normalization (the per-doc length is *stored* in the index but discarded).
//!   B. a disjunctive `a OR b` query takes `max` of per-term scores, not the sum,
//!      so a doc matching *both* terms can tie a doc matching one term strongly.
//!
//! Uses the **sqlite** backend (stored path) per CLAUDE.md's test-backend rule.

use cozo::{DbInstance, NamedRows, ScriptMutability};
use std::collections::{BTreeMap, HashMap};

fn db() -> DbInstance {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bm25.db");
    // Leak the tempdir so the sqlite file outlives this fn for the test's duration.
    std::mem::forget(dir);
    DbInstance::new("sqlite", path.to_str().unwrap(), Default::default()).unwrap()
}

fn run(db: &DbInstance, s: &str) -> NamedRows {
    db.run_script(s, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("script failed: {e:?}\n--- script ---\n{s}"))
}

/// Build a doc relation + FTS index with the given rows, no stemming/stopwords so
/// our synthetic tokens survive verbatim.
fn setup(db: &DbInstance, rows: &str) {
    run(db, r":create doc {k: String => body: String}");
    run(
        db,
        r"::fts create doc:fts { extractor: body, tokenizer: Simple, filters: [Lowercase] }",
    );
    run(
        db,
        &format!(r"?[k, body] <- [{rows}] :put doc {{k => body}}"),
    );
}

/// query -> {doc_key: score}, using the given fts param tail (after `|`).
fn scores(db: &DbInstance, params: &str) -> HashMap<String, f64> {
    let res = run(
        db,
        &format!(r"?[k, s] := ~doc:fts{{k, body | {params}, bind_score: s}}"),
    );
    res.rows
        .iter()
        .map(|r| {
            (
                r[0].get_str().unwrap().to_string(),
                r[1].get_float().unwrap(),
            )
        })
        .collect()
}

/// Defect B: an `OR` of two terms must SUM per-term contributions, so a doc
/// matching both terms outranks one matching a single term many times (whose tf
/// saturates under BM25's k1). Under upstream `tf_idf` + max-OR, `single` wins.
#[test]
fn bm25_or_sums_per_term_contributions() {
    let db = db();
    setup(
        &db,
        r"
        ['multi', 'alpha beta'],
        ['single', 'alpha alpha alpha alpha alpha alpha']
    ",
    );
    let s = scores(&db, "query: 'alpha OR beta', k: 10");
    let multi = s["multi"];
    let single = s["single"];
    assert!(
        multi > single,
        "doc matching both OR terms ({multi}) must outrank doc matching one term \
         repeatedly ({single}) — OR should sum, and repeated tf should saturate"
    );
}

/// Defect A: BM25 length-normalizes. Two docs with identical term frequency (1)
/// for the query term but different lengths must NOT tie — the shorter doc scores
/// higher. Upstream `tf_idf` ignores doc length, so they tie.
#[test]
fn bm25_length_normalization_favors_shorter_doc() {
    let db = db();
    let filler = vec!["pad"; 60].join(" ");
    setup(
        &db,
        &format!(
            r"
        ['short', 'gamma'],
        ['long', 'gamma {filler}']
    "
        ),
    );
    let s = scores(&db, "query: 'gamma', k: 10");
    let short = s["short"];
    let long = s["long"];
    assert!(
        short > long,
        "with equal tf, the shorter doc ({short}) must outrank the longer, \
         length-diluted doc ({long}) under BM25 length normalization"
    );
}

/// `k1`/`b` parameters must parse and influence scoring. With `b: 0.0` (length
/// normalization disabled), the length-difference docs tie again — proving `b`
/// is actually wired through, not ignored.
#[test]
fn bm25_b_zero_disables_length_normalization() {
    let db = db();
    let filler = vec!["pad"; 60].join(" ");
    setup(
        &db,
        &format!(
            r"
        ['short', 'gamma'],
        ['long', 'gamma {filler}']
    "
        ),
    );
    let s = scores(&db, "query: 'gamma', k: 10, b: 0.0");
    assert!(
        (s["short"] - s["long"]).abs() < 1e-9,
        "with b=0 (no length norm) equal-tf docs must tie: short={}, long={}",
        s["short"],
        s["long"]
    );
}
