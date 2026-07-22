/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Tests for the mnestic fork's **durable FTS doc-stats counter** (DEVELOPMENT.md
//! Bet 1b, avgdl step). BM25 length normalization needs the average document
//! length (`avgdl`); upstream's only option was a full deduplicated scan of the
//! FTS index on every query (O(#docs), ~680 ms at 40k chunks in the bench). The
//! fork maintains `(total_tokens, n_docs)` incrementally on `put`/`del` and at
//! index build, so `avgdl` is an O(1) read.
//!
//! These tests pin the *behaviour* that proves the counter is maintained
//! correctly (not just fast): scores are identical whether a corpus is reached by
//! deletes or built fresh, the counter survives a reopen, and `avgdl` actually
//! feeds the BM25 denominator. Uses the **sqlite** backend (real stored path).

use cozo::{DbInstance, NamedRows, ScriptMutability};
use std::collections::{BTreeMap, HashMap};

fn run(db: &DbInstance, s: &str) -> NamedRows {
    db.run_script(s, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("script failed: {e:?}\n--- script ---\n{s}"))
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

fn rm(db: &DbInstance, keys: &str) {
    run(db, &format!(r"?[k] <- [{keys}] :rm doc {{k}}"));
}

/// `{doc_key: bm25_score}` for a single-term query (bm25 is the default scorer).
fn scores(db: &DbInstance, query: &str) -> HashMap<String, f64> {
    let res = run(
        db,
        &format!(r"?[k, s] := ~doc:fts{{k, body | query: '{query}', k: 50, bind_score: s}}"),
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

fn new_db(path: &std::path::Path) -> DbInstance {
    DbInstance::new("sqlite", path.to_str().unwrap(), Default::default()).unwrap()
}

const FILLER: &str =
    "pad pad pad pad pad pad pad pad pad pad pad pad pad pad pad pad pad pad pad pad";

/// The counter must net correctly through deletes: a corpus reached by inserting
/// extra long documents and then deleting them must score *identically* to the
/// same corpus built fresh. `gamma` lives only in the short doc, so `df`/`N` match
/// across the two databases — any score difference would be a stale `avgdl`, i.e.
/// a delete that failed to subtract from the counter.
#[test]
fn delete_maintains_avgdl_like_fresh_build() {
    let dir = tempfile::tempdir().unwrap();

    // A: insert short + long docs, then delete the long ones.
    let a = new_db(&dir.path().join("a.db"));
    setup(&a);
    put(
        &a,
        &format!(
            r"['short', 'gamma'], ['l1', 'delta {FILLER}'], ['l2', 'delta {FILLER}'], ['l3', 'delta {FILLER}']"
        ),
    );
    rm(&a, r"['l1'], ['l2'], ['l3']");
    let sa = scores(&a, "gamma");

    // B: build the end state (just the short doc) directly.
    let b = new_db(&dir.path().join("b.db"));
    setup(&b);
    put(&b, r"['short', 'gamma']");
    let sb = scores(&b, "gamma");

    assert!(
        (sa["short"] - sb["short"]).abs() < 1e-9,
        "score after deletes ({}) must equal fresh-built score ({}) — a difference \
         means the delete did not maintain the avgdl counter",
        sa["short"],
        sb["short"]
    );
}

/// The counter is durable: scores are identical after closing and reopening the
/// database (the counter is read back from the store, not recomputed differently).
#[test]
fn avgdl_counter_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reopen.db");

    let before = {
        let db = new_db(&path);
        setup(&db);
        put(
            &db,
            &format!(r"['short', 'gamma'], ['long', 'gamma {FILLER}']"),
        );
        scores(&db, "gamma")
    }; // db dropped here → sqlite file flushed

    let db = new_db(&path);
    let after = scores(&db, "gamma");

    for k in ["short", "long"] {
        assert!(
            (before[k] - after[k]).abs() < 1e-9,
            "score for {k} changed across reopen: {} -> {}",
            before[k],
            after[k]
        );
    }
}

/// Bulk build vs incremental build on a relation with a **multi-column primary
/// key**: `::fts create` over existing rows seeds the doc-stats counter from
/// build counts (`seed_fts_doc_stats`), relying on "distinct base tuples ⇒
/// distinct FTS doc keys". A composite PK is where that invariant would break
/// if doc keys ever collapsed to a single column — scores would diverge from
/// the incremental path, which bumps the counter once per put.
#[test]
fn bulk_build_doc_stats_match_incremental_on_multi_column_pk() {
    let dir = tempfile::tempdir().unwrap();

    let rows = format!(
        r"['a', 1, 'gamma'], ['a', 2, 'delta {FILLER}'], ['b', 1, 'gamma delta'], ['b', 2, 'epsilon {FILLER}']"
    );
    let setup_rel = |db: &DbInstance| {
        run(db, r":create note {ns: String, seq: Int => body: String}");
    };
    let create_idx = |db: &DbInstance| {
        run(
            db,
            r"::fts create note:fts { extractor: body, tokenizer: Simple, filters: [Lowercase] }",
        );
    };
    let put_rows = |db: &DbInstance| {
        run(
            db,
            &format!(r"?[ns, seq, body] <- [{rows}] :put note {{ns, seq => body}}"),
        );
    };
    let scores = |db: &DbInstance, query: &str| -> Vec<(String, i64, f64)> {
        let res = run(
            db,
            &format!(
                r"?[ns, seq, s] := ~note:fts{{ns, seq | query: '{query}', k: 50, bind_score: s}} :order ns, seq"
            ),
        );
        res.rows
            .iter()
            .map(|r| {
                (
                    r[0].get_str().unwrap().to_string(),
                    r[1].get_int().unwrap(),
                    r[2].get_float().unwrap(),
                )
            })
            .collect()
    };

    // A: bulk path — rows exist before the index is created (seeded stats).
    let bulk = new_db(&dir.path().join("bulk.db"));
    setup_rel(&bulk);
    put_rows(&bulk);
    create_idx(&bulk);

    // B: incremental path — index created empty, rows put afterwards.
    let incr = new_db(&dir.path().join("incr.db"));
    setup_rel(&incr);
    create_idx(&incr);
    put_rows(&incr);

    for q in ["gamma", "delta", "epsilon"] {
        let sa = scores(&bulk, q);
        let sb = scores(&incr, q);
        assert!(
            !sa.is_empty(),
            "query '{q}' returned no hits on the bulk build"
        );
        assert_eq!(
            sa.len(),
            sb.len(),
            "hit count differs for '{q}': bulk {sa:?} vs incremental {sb:?}"
        );
        for ((ns_a, seq_a, s_a), (ns_b, seq_b, s_b)) in sa.iter().zip(&sb) {
            assert_eq!((ns_a, seq_a), (ns_b, seq_b), "doc keys differ for '{q}'");
            assert!(
                (s_a - s_b).abs() < 1e-9,
                "score for ({ns_a}, {seq_a}) on '{q}' differs: bulk {s_a} vs incremental {s_b} \
                 — seeded doc-stats diverge from incrementally maintained ones"
            );
        }
    }
}

/// `avgdl` must actually feed the BM25 length normalization: a document of fixed
/// length scores *higher* in a corpus with a larger average document length
/// (its `|D|/avgdl` ratio shrinks, so the length penalty relaxes). `gamma` has
/// `df = 1` and `N = 2` in both corpora, so only `avgdl` differs.
#[test]
fn avgdl_value_feeds_bm25_denominator() {
    let dir = tempfile::tempdir().unwrap();

    // Small avgdl: the other document is short.
    let small = new_db(&dir.path().join("small.db"));
    setup(&small);
    put(&small, r"['target', 'gamma'], ['other', 'delta']");
    let s_small = scores(&small, "gamma")["target"];

    // Large avgdl: the other document is long (same N, same df for 'gamma').
    let large = new_db(&dir.path().join("large.db"));
    setup(&large);
    put(
        &large,
        &format!(r"['target', 'gamma'], ['other', 'delta {FILLER}']"),
    );
    let s_large = scores(&large, "gamma")["target"];

    assert!(
        s_large > s_small,
        "the fixed-length 'target' doc must score higher when avgdl is larger \
         (small avgdl: {s_small}, large avgdl: {s_large}) — proving avgdl is used"
    );
}
