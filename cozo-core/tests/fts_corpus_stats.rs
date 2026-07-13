/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Tests for BM25's **corpus size `N`** — the sibling of `fts_avgdl.rs`, and the half of the
//! 0.8.4 "O(1) avgdl" work that was left behind.
//!
//! `avgdl` was made an O(1) read off the process-level doc-stats cache. `N` was not: it kept
//! calling `FtsCache::get_n_for_relation`, which `range_count`s the **entire base relation** on
//! **every FTS query**. That was two bugs in one:
//!
//!   * **Wrong.** IDF is `ln(1 + (N - df + 0.5) / (df + 0.5))`. `df` is counted from the FTS
//!     *index*, and `avgdl` is averaged over the FTS *index* — but `N` was counted over the base
//!     *relation*. A row carrying no tokens (empty text) is a row in the relation but not a
//!     document in the collection being searched, so it inflated `N` and depressed every IDF.
//!     Rows bulk-loaded by `import_relations` (which maintains no FTS index) did the same, at
//!     arbitrary magnitude.
//!   * **Slow.** O(corpus) per query. Measured on hybrid-recall-bench (RocksDB, real embeddings):
//!     the FTS leg cost 2,621 ms at 400k chunks against 350 ms for the HNSW leg, because counting
//!     the rows drags the relation's whole value payload through the block cache.
//!
//! `N` now comes from the same doc-stats cache as `avgdl`: the number of documents carrying at
//! least one posting.
//!
//! **These tests discriminate.** Restore `get_n_for_relation` and they go red — the scores move,
//! because the two databases in each test disagree on `N` precisely when unindexed rows exist.
//! Uses the **sqlite** backend (the real stored path; `mem` uses a different join operator).

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

/// The three documents that actually carry postings. Identical in both databases of every test,
/// so `df` and every document length are identical too: **`N` is the only thing that can differ.**
const INDEXED: &str = r#"['a', 'gamma delta'], ['b', 'delta epsilon'], ['c', 'delta zeta']"#;

fn assert_scores_eq(left: &HashMap<String, f64>, right: &HashMap<String, f64>, ctx: &str) {
    assert_eq!(
        left.len(),
        right.len(),
        "{ctx}: different number of hits ({left:?} vs {right:?})"
    );
    for (k, l) in left {
        let r = right
            .get(k)
            .unwrap_or_else(|| panic!("{ctx}: key {k:?} missing from the right-hand result"));
        assert!(
            (l - r).abs() < 1e-12,
            "{ctx}: BM25 score for {k:?} differs: {l} vs {r}. \
             The only quantity that can differ between these two corpora is N, so this is N \
             being sourced from the base relation instead of the FTS index."
        );
    }
}

/// **The discriminating test.** Two databases hold the *same three indexed documents*. One of
/// them also holds 200 rows whose body is empty — rows in the relation, but not documents in the
/// index, so they carry no postings.
///
/// BM25's IDF is a pure function of `N` and `df` here (`df` and doc lengths are pinned by
/// construction). Post-fix, `N = 3` in both, so the scores must be *identical*. Pre-fix, `N` came
/// from `range_count` over the base relation: 3 versus 203. Restore `get_n_for_relation` and this
/// assertion fails.
#[test]
fn unindexed_rows_do_not_change_bm25_scores() {
    let dir = tempfile::tempdir().unwrap();

    let lean = new_db(&dir.path().join("lean.db"));
    setup(&lean);
    put(&lean, INDEXED);

    let padded = new_db(&dir.path().join("padded.db"));
    setup(&padded);
    put(&padded, INDEXED);
    // 200 rows that tokenize to nothing: present in `doc`, absent from `doc:fts`.
    let empties = (0..200)
        .map(|i| format!("['pad{i}', '']"))
        .collect::<Vec<_>>()
        .join(", ");
    put(&padded, &empties);

    // Sanity: the padding really did land in the base relation.
    let n_rows = run(&padded, r"?[count(k)] := *doc{k}").rows[0][0]
        .get_int()
        .unwrap();
    assert_eq!(
        n_rows, 203,
        "the empty-body rows must exist in the relation"
    );

    // ...and really did not land in the index: same hits as the lean DB.
    let lean_scores = scores(&lean, "delta");
    let padded_scores = scores(&padded, "delta");
    assert_eq!(lean_scores.len(), 3, "expected the 3 indexed docs to match");

    assert_scores_eq(&lean_scores, &padded_scores, "empty-body padding");
}

/// The same invariant on the axis that made the old `N` arbitrarily wrong rather than merely
/// slightly wrong: a document whose body is *whitespace only* also produces no postings. A corpus
/// padded with such rows must score exactly like one without them.
#[test]
fn whitespace_only_rows_do_not_change_bm25_scores() {
    let dir = tempfile::tempdir().unwrap();

    let lean = new_db(&dir.path().join("lean.db"));
    setup(&lean);
    put(&lean, INDEXED);

    let padded = new_db(&dir.path().join("padded.db"));
    setup(&padded);
    put(&padded, INDEXED);
    let empties = (0..50)
        .map(|i| format!("['ws{i}', '   ']"))
        .collect::<Vec<_>>()
        .join(", ");
    put(&padded, &empties);

    assert_scores_eq(
        &scores(&lean, "delta"),
        &scores(&padded, "delta"),
        "whitespace-only padding",
    );
}

/// `N` must be the *index's* document count, so it has to track writes to the index the same way
/// `avgdl` does. Adding a genuinely-indexed document changes `N` and therefore must move the
/// scores — this pins that the new source of `N` is live, not frozen at first touch (a cache that
/// seeded once and never updated would pass the two tests above and be badly wrong here).
#[test]
fn indexed_writes_do_move_n() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(&dir.path().join("a.db"));
    setup(&db);
    put(&db, INDEXED);

    let before = scores(&db, "delta");

    // 'delta' is deliberately absent, so df is unchanged and only N moves.
    put(
        &db,
        r#"['d', 'omicron'], ['e', 'omicron'], ['f', 'omicron']"#,
    );
    let after = scores(&db, "delta");

    assert_eq!(before.len(), 3);
    assert_eq!(after.len(), 3, "df must not have changed");
    let (a, b) = (before["a"], after["a"]);
    assert!(
        (a - b).abs() > 1e-9,
        "adding indexed documents must change N and therefore the IDF \
         (before={a}, after={b}); a frozen N would leave the score untouched"
    );
}

/// **The second bug this fix exposed.** A value-unchanged `:put` is a no-op for the index —
/// `query/stored.rs` skips the posting delete when `extracted == tup`, because an identical tuple
/// derives identical postings. But the *put* side still ran, bumping the doc-stats counter `+1`
/// with no matching `-1`, so the document count drifted up on every no-op write.
///
/// It was invisible for as long as `avgdl = total / n` was the counter's only consumer: a
/// re-put inflates `total` and `n` together and the ratio barely moves. BM25's `N` reads `n`
/// directly, so the drift became a score error. Re-putting the same rows must not move any score.
#[test]
fn repeated_identical_puts_do_not_drift_n() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(&dir.path().join("reput.db"));
    setup(&db);
    put(&db, INDEXED);

    let once = scores(&db, "delta");

    // Ten no-op writes. Nothing about the corpus changes, so nothing about BM25 may change.
    for _ in 0..10 {
        put(&db, INDEXED);
    }
    let after = scores(&db, "delta");

    assert_scores_eq(&once, &after, "ten identical re-puts");
}
