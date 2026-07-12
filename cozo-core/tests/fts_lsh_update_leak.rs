/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Regression tests for the **FTS posting leak on `:put` update** (0.12.1).
//!
//! `query/stored.rs` gated `del_in_fts`/`del_in_lsh` behind `has_indices` — which
//! means *plain B-tree secondary indexes only*. A relation carrying **only** an
//! FTS index therefore never deleted the old document's postings when a row was
//! updated in place: terms the document no longer contains kept matching it
//! (ghost hits), the index grew without bound, and the BM25 `df`/`avgdl`
//! statistics skewed. The `rm` path always deleted correctly, and so did the
//! `update` op — only `:put`-over-an-existing-key leaked.
//!
//! **LSH sat behind the same gate but does *not* leak** — `put_lsh_index_item`
//! is self-cleaning (see `lsh_only_update_evicts_old_signature` below, which is a
//! characterization pin, not a regression test: it was green before the fix). The
//! original audit reported this as an "FTS/LSH" leak; writing the LSH test first
//! and watching it pass is what corrected that. The file keeps its name so the
//! LSH pin has an obvious home.
//!
//! Every test here is written against a relation with **no** B-tree secondary
//! index, because that is precisely the configuration the bug required. The
//! `btree_plus_fts_*` test pins the path that already worked, so the fix cannot
//! regress it. Uses the **sqlite** backend (the real stored path — `mem` uses a
//! different join operator and would not exercise this code).

use cozo::{DbInstance, NamedRows, ScriptMutability};
use std::collections::BTreeMap;

fn run(db: &DbInstance, s: &str) -> NamedRows {
    db.run_script(s, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("script failed: {e:?}\n--- script ---\n{s}"))
}

fn new_db(path: &std::path::Path) -> DbInstance {
    DbInstance::new("sqlite", path.to_str().unwrap(), Default::default()).unwrap()
}

/// Keys matching a single-term FTS query.
fn fts_hits(db: &DbInstance, query: &str) -> Vec<String> {
    let res = run(
        db,
        &format!(r"?[k] := ~doc:fts{{k, body | query: '{query}', k: 50}} :order k"),
    );
    res.rows
        .iter()
        .map(|r| r[0].get_str().unwrap().to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// FTS
// ---------------------------------------------------------------------------

/// The bug, stated directly: update a row's text on an **FTS-only** relation and
/// the old terms must stop matching it.
///
/// Discrimination: revert the `stored.rs` gate fix and `hello` still returns
/// `k1` — a term the document no longer contains.
#[test]
fn fts_only_update_evicts_old_postings() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(&dir.path().join("fts.db"));

    run(&db, r":create doc {k: String => body: String}");
    run(
        &db,
        r"::fts create doc:fts { extractor: body, tokenizer: Simple, filters: [Lowercase] }",
    );

    run(
        &db,
        r"?[k, body] <- [['k1', 'hello world']] :put doc {k => body}",
    );
    assert_eq!(
        fts_hits(&db, "hello"),
        vec!["k1"],
        "sanity: the term indexes"
    );

    // Update in place: the document no longer contains 'hello' or 'world'.
    run(
        &db,
        r"?[k, body] <- [['k1', 'goodbye moon']] :put doc {k => body}",
    );

    assert_eq!(
        fts_hits(&db, "goodbye"),
        vec!["k1"],
        "the new term must index"
    );
    assert!(
        fts_hits(&db, "hello").is_empty(),
        "GHOST POSTING: 'hello' still matches k1 after the row was updated to \
         'goodbye moon' — the old document's postings were never deleted"
    );
    assert!(
        fts_hits(&db, "world").is_empty(),
        "GHOST POSTING: 'world' still matches k1 after the update"
    );
}

/// The leak also skewed BM25 document statistics: a leaked update leaves the old
/// token count in the `(total_tokens, n_docs)` counter, so `avgdl` drifts. A
/// corpus reached by updating a document must score identically to the same
/// corpus built fresh.
///
/// Discrimination: revert the fix and the updated-corpus score diverges from the
/// fresh-built one (the stale tokens inflate `avgdl`).
#[test]
fn fts_only_update_maintains_avgdl() {
    const FILLER: &str =
        "pad pad pad pad pad pad pad pad pad pad pad pad pad pad pad pad pad pad pad pad";
    let dir = tempfile::tempdir().unwrap();

    let setup = |db: &DbInstance| {
        run(db, r":create doc {k: String => body: String}");
        run(
            db,
            r"::fts create doc:fts { extractor: body, tokenizer: Simple, filters: [Lowercase] }",
        );
    };
    let score = |db: &DbInstance| -> f64 {
        let res = run(
            db,
            r"?[k, s] := ~doc:fts{k, body | query: 'gamma', k: 50, bind_score: s}",
        );
        res.rows[0][1].get_float().unwrap()
    };

    // A: 'other' starts long, then is updated to be short.
    let a = new_db(&dir.path().join("a.db"));
    setup(&a);
    run(
        &a,
        &format!(
            r"?[k, body] <- [['target', 'gamma'], ['other', 'delta {FILLER}']] :put doc {{k => body}}"
        ),
    );
    run(
        &a,
        r"?[k, body] <- [['other', 'delta']] :put doc {k => body}",
    );
    let sa = score(&a);

    // B: the same end state, built fresh.
    let b = new_db(&dir.path().join("b.db"));
    setup(&b);
    run(
        &b,
        r"?[k, body] <- [['target', 'gamma'], ['other', 'delta']] :put doc {k => body}",
    );
    let sb = score(&b);

    assert!(
        (sa - sb).abs() < 1e-9,
        "score after an in-place update ({sa}) must equal the fresh-built score ({sb}) — \
         a difference means the update left the old document's tokens in the doc-stats \
         counter, skewing avgdl"
    );
}

/// The path that already worked must keep working: with a plain secondary index
/// present, posting deletion was correctly armed. This is the no-regression pin
/// for the gate restructure.
#[test]
fn btree_plus_fts_update_still_evicts() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(&dir.path().join("both.db"));

    run(&db, r":create doc {k: String => body: String, tag: String}");
    run(&db, r"::index create doc:by_tag {tag}");
    run(
        &db,
        r"::fts create doc:fts { extractor: body, tokenizer: Simple, filters: [Lowercase] }",
    );

    run(
        &db,
        r"?[k, body, tag] <- [['k1', 'hello world', 'a']] :put doc {k => body, tag}",
    );
    run(
        &db,
        r"?[k, body, tag] <- [['k1', 'goodbye moon', 'a']] :put doc {k => body, tag}",
    );

    assert_eq!(fts_hits(&db, "goodbye"), vec!["k1"]);
    assert!(
        fts_hits(&db, "hello").is_empty(),
        "regression: the B-tree + FTS path previously deleted old postings correctly"
    );
}

/// A no-op re-put (identical tuple) must not disturb the index: the fix keeps the
/// `extracted != tup` value-unchanged skip, since an identical tuple derives
/// identical postings.
#[test]
fn fts_only_identical_reput_is_stable() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(&dir.path().join("noop.db"));

    run(&db, r":create doc {k: String => body: String}");
    run(
        &db,
        r"::fts create doc:fts { extractor: body, tokenizer: Simple, filters: [Lowercase] }",
    );

    run(
        &db,
        r"?[k, body] <- [['k1', 'hello world']] :put doc {k => body}",
    );
    let before = run(
        &db,
        r"?[k, s] := ~doc:fts{k, body | query: 'hello', k: 50, bind_score: s}",
    );
    run(
        &db,
        r"?[k, body] <- [['k1', 'hello world']] :put doc {k => body}",
    );
    let after = run(
        &db,
        r"?[k, s] := ~doc:fts{k, body | query: 'hello', k: 50, bind_score: s}",
    );

    assert_eq!(
        before.rows, after.rows,
        "an identical re-put changed the index"
    );
}

// ---------------------------------------------------------------------------
// LSH
// ---------------------------------------------------------------------------

/// **LSH does not leak, and this test exists to keep it that way.**
///
/// `del_in_lsh` sat behind the same `has_indices` gate as `del_in_fts`, so LSH
/// *looks* like it should leak identically — the 2026-07 upstream audit and the
/// roadmap both recorded it as "the FTS/**LSH** posting leak". It does not, and
/// this test was **green before the fix**: `put_lsh_index_item`
/// (`runtime/minhash_lsh.rs:82-96`) is self-cleaning — it looks the row's
/// existing signature up in the inverted index and deletes the old bands before
/// writing the new ones — so the gated `del_in_lsh` was only ever redundant.
///
/// This is therefore a **characterization pin, not a regression test**: it has no
/// discriminating power against the `stored.rs` gate (that is the point), and it
/// fails only if someone removes LSH's self-clean. Keep it for that.
#[test]
fn lsh_only_update_evicts_old_signature() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(&dir.path().join("lsh.db"));

    run(&db, r":create doc {k: String => body: String}");
    run(
        &db,
        r"::lsh create doc:lsh {
            extractor: body, tokenizer: Simple, filters: [Lowercase],
            n_perm: 64, target_threshold: 0.5, n_gram: 3
        }",
    );

    let near_dup = |db: &DbInstance, text: &str| -> Vec<String> {
        let res = run(
            db,
            &format!(r"?[k] := ~doc:lsh{{k, body | query: '{text}', k: 50}} :order k"),
        );
        res.rows
            .iter()
            .map(|r| r[0].get_str().unwrap().to_string())
            .collect()
    };

    let old_text = "the quick brown fox jumps over the lazy dog";
    let new_text = "entirely unrelated content about databases and indexes";

    run(
        &db,
        &format!(r"?[k, body] <- [['k1', '{old_text}']] :put doc {{k => body}}"),
    );
    assert_eq!(
        near_dup(&db, old_text),
        vec!["k1"],
        "sanity: the document is its own near-duplicate"
    );

    run(
        &db,
        &format!(r"?[k, body] <- [['k1', '{new_text}']] :put doc {{k => body}}"),
    );

    assert_eq!(
        near_dup(&db, new_text),
        vec!["k1"],
        "the new signature must index"
    );
    assert!(
        near_dup(&db, old_text).is_empty(),
        "GHOST SIGNATURE: the pre-update text still finds k1 — the old minhash \
         bands were never deleted"
    );
}
