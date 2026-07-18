/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Index-search diagnostics carry the code of the index kind that actually
//! failed (upstream #231/#257; 0.14.0 item (e)). Before this suite, an FTS
//! search with a missing `query:` said "required for HNSW search" with an
//! `hnsw_query_required` code, the LSH normalizer reused two HNSW codes, and
//! the generic index-not-found fall-through — which fires when *no* index of
//! any kind matched — was labeled `eval::hnsw_index_not_found`.
//!
//! These are the only tests in the tree that assert on diagnostic *codes*;
//! the codes are user-visible surface for agents that branch on them.

use cozo::DbInstance;

/// miette's fancy Debug rendering includes the diagnostic code; collapse the
/// box-drawing decorations so `contains` checks are robust.
fn err(db: &DbInstance, s: &str) -> String {
    let e = db.run_default(s).unwrap_err();
    format!("{e:?}")
        .replace(['\u{2502}', '\u{d7}'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn make_db() -> DbInstance {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create docs {id: String => text: String, emb: <F32; 2>}")
        .unwrap();
    db.run_default(
        "?[id, text, emb] <- [['d1', 'hello world', [1.0, 0.0]]] :put docs {id => text, emb}",
    )
    .unwrap();
    db.run_default(
        "::fts create docs:fts { extractor: text, tokenizer: Simple, filters: [Lowercase] }",
    )
    .unwrap();
    db.run_default(
        "::lsh create docs:lsh { extractor: text, tokenizer: Simple, n_gram: 3, \
         target_threshold: 0.3 }",
    )
    .unwrap();
    db.run_default(
        "::hnsw create docs:vec { dim: 2, m: 50, dtype: F32, fields: [emb], \
         distance: L2, ef_construction: 20 }",
    )
    .unwrap();
    db
}

#[test]
fn fts_missing_query_carries_the_fts_code() {
    let db = make_db();
    let m = err(&db, "?[id] := ~docs:fts{ id | k: 3 }");
    assert!(m.contains("parser::fts_query_required"), "{m}");
    assert!(m.contains("FTS search"), "{m}");
    assert!(!m.contains("HNSW"), "an FTS failure blames HNSW: {m}");
}

#[test]
fn fts_bad_k_carries_the_fts_code() {
    let db = make_db();
    let m = err(&db, "?[id] := ~docs:fts{ id | query: 'hello', k: true }");
    assert!(m.contains("parser::expected_int_for_fts_k"), "{m}");
}

#[test]
fn lsh_missing_query_carries_the_lsh_code() {
    let db = make_db();
    let m = err(&db, "?[id] := ~docs:lsh{ id | k: 3 }");
    assert!(m.contains("parser::lsh_query_required"), "{m}");
    assert!(!m.contains("hnsw"), "an LSH failure blames HNSW: {m}");
}

#[test]
fn lsh_bad_k_carries_the_lsh_code() {
    let db = make_db();
    let m = err(&db, "?[id] := ~docs:lsh{ id | query: 'hello', k: true }");
    assert!(m.contains("parser::expected_int_for_lsh_k"), "{m}");
}

#[test]
fn unknown_index_is_the_generic_code() {
    let db = make_db();
    let m = err(&db, "?[id] := ~docs:nosuch{ id | query: 'hello', k: 3 }");
    assert!(m.contains("eval::index_not_found"), "{m}");
    assert!(
        !m.contains("hnsw_index_not_found"),
        "the generic fall-through blames HNSW: {m}"
    );
}

/// The genuinely-HNSW diagnostics keep their HNSW codes (the fix relabeled
/// only the cross-index reuses).
#[test]
fn hnsw_missing_query_keeps_the_hnsw_code() {
    let db = make_db();
    let m = err(&db, "?[id] := ~docs:vec{ id | k: 3, ef: 20 }");
    assert!(m.contains("parser::hnsw_query_required"), "{m}");
}
