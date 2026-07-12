/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `::reindex <relation>` — rebuild HNSW/FTS/LSH indexes in place from their
//! stored manifests (mnestic fork, 0.12.1).
//!
//! The correctness argument is an **equivalence**: a rebuilt index must be
//! indistinguishable from one that was maintained incrementally all along. Every
//! test here is a form of that claim —
//!
//! - `*_repairs_*`: an index that has drifted from its base relation (stranded by
//!   a bulk import, or carrying the ghost postings of the pre-0.12.1 leak) is made
//!   correct again;
//! - `*_equivalence*`: the rebuilt index answers queries identically to a fresh
//!   one built over the same rows;
//! - `*_is_idempotent`: rebuilding twice changes nothing.
//!
//! Uses the **sqlite** backend (the real stored path).

use std::collections::BTreeMap;

use cozo::{DbInstance, NamedRows, ScriptMutability};

fn run(db: &DbInstance, s: &str) -> NamedRows {
    db.run_script(s, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("script failed: {e:?}\n--- script ---\n{s}"))
}

fn new_db(path: &std::path::Path) -> DbInstance {
    DbInstance::new("sqlite", path.to_str().unwrap(), Default::default()).unwrap()
}

fn create_doc(db: &DbInstance) {
    run(db, r":create doc {k: String => body: String}");
}

fn create_fts(db: &DbInstance) {
    run(
        db,
        r"::fts create doc:fts { extractor: body, tokenizer: Simple, filters: [Lowercase] }",
    );
}

/// `{key: score}` for a single-term FTS query — the score matters, because a
/// correct rebuild must also restore the BM25 corpus statistics, not merely the
/// postings.
fn fts_scores(db: &DbInstance, query: &str) -> Vec<(String, f64)> {
    run(
        db,
        &format!(
            r"?[k, s] := ~doc:fts{{k, body | query: '{query}', k: 50, bind_score: s}} :order k"
        ),
    )
    .rows
    .iter()
    .map(|r| {
        (
            r[0].get_str().unwrap().to_string(),
            r[1].get_float().unwrap(),
        )
    })
    .collect()
}

// ---------------------------------------------------------------------------
// The three stories `::reindex` exists to close
// ---------------------------------------------------------------------------

/// **Story 1: the posting leak's repair path.** A database that updated rows in
/// place on an FTS-only relation *before* 0.12.1 carries ghost postings today —
/// the write-path fix stops new leakage but cannot evict what is already written.
///
/// The leak is simulated exactly as it occurred: postings written for a document
/// version that no longer exists. We do that by building the index while the old
/// text is live, then rewriting the row's value underneath it via a bulk import
/// (which does not touch the index) — leaving the index holding the *old*
/// document's terms while the relation holds the new one. That is precisely the
/// end state the leak produced.
///
/// Discrimination: skip the `::reindex` and `hello` still matches `k1`.
#[test]
fn reindex_evicts_ghost_postings() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(&dir.path().join("ghost.db"));

    create_doc(&db);
    // The ghost document is deliberately a DIFFERENT LENGTH from the one that
    // replaces it (5 tokens vs 2). Equal lengths would make this test blind to
    // half the corruption: the BM25 corpus counter is consumed as the ratio
    // `total_tokens / n_docs`, so a stale counter that double-counts an
    // equal-length document yields the *same* avgdl and hides itself.
    run(
        &db,
        r"?[k, body] <- [['k1', 'hello world alpha beta gamma']] :put doc {k => body}",
    );
    create_fts(&db);
    assert_eq!(
        fts_scores(&db, "hello").len(),
        1,
        "sanity: the term indexes"
    );

    // Rewrite the row without touching the index — the index now holds a
    // document version the relation no longer has.
    db.import_relations(BTreeMap::from([(
        "doc".to_string(),
        NamedRows::new(
            vec!["k".to_string(), "body".to_string()],
            vec![vec![
                cozo::DataValue::from("k1"),
                cozo::DataValue::from("goodbye moon"),
            ]],
        ),
    )]))
    .unwrap();

    assert_eq!(
        fts_scores(&db, "hello").len(),
        1,
        "precondition: the ghost posting exists (the index still matches the old text)"
    );

    run(&db, r"::reindex doc");

    assert!(
        fts_scores(&db, "hello").is_empty(),
        "GHOST SURVIVED THE REBUILD: 'hello' still matches k1, but the relation now \
         says 'goodbye moon' — ::reindex did not evict the stale postings"
    );
    assert_eq!(
        fts_scores(&db, "goodbye").len(),
        1,
        "the rebuilt index must find the current text"
    );

    // And the BM25 *statistics* must be repaired too, not just the postings. This
    // is the assertion that catches a rebuild which re-derives the postings while
    // leaving the corpus counter (`total_tokens`, `n_docs`) carrying the ghost
    // document's contribution — the index would then answer with the right hits
    // and the wrong scores, which is the subtler half of the same corruption.
    let clean = new_db(&dir.path().join("clean.db"));
    create_doc(&clean);
    run(
        &clean,
        r"?[k, body] <- [['k1', 'goodbye moon']] :put doc {k => body}",
    );
    create_fts(&clean);

    let repaired = fts_scores(&db, "goodbye");
    let reference = fts_scores(&clean, "goodbye");
    assert!(
        (repaired[0].1 - reference[0].1).abs() < 1e-9,
        "the rebuilt index scores '{}' at {} but a clean index over the same single \
         document scores it at {} — the corpus statistics still carry the ghost \
         document, so avgdl/IDF are wrong",
        repaired[0].0,
        repaired[0].1,
        reference[0].1
    );
}

/// **Story 2: the bulk-load paths.** `import_relations` does not maintain
/// HNSW/FTS/LSH — imported rows are invisible to search until a rebuild. That is
/// what its warning now tells the user to run.
#[test]
fn reindex_makes_bulk_imported_rows_searchable() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(&dir.path().join("import.db"));

    create_doc(&db);
    create_fts(&db);

    db.import_relations(BTreeMap::from([(
        "doc".to_string(),
        NamedRows::new(
            vec!["k".to_string(), "body".to_string()],
            vec![
                vec![
                    cozo::DataValue::from("k1"),
                    cozo::DataValue::from("alpha beta"),
                ],
                vec![
                    cozo::DataValue::from("k2"),
                    cozo::DataValue::from("beta gamma"),
                ],
            ],
        ),
    )]))
    .unwrap();

    assert!(
        fts_scores(&db, "beta").is_empty(),
        "precondition: bulk-imported rows are invisible to FTS"
    );

    let report = run(&db, r"::reindex doc");
    assert_eq!(report.rows.len(), 1, "one index rebuilt");
    assert_eq!(report.rows[0][0].get_str().unwrap(), "fts");
    assert_eq!(report.rows[0][2].get_int().unwrap(), 2, "two rows indexed");

    assert_eq!(
        fts_scores(&db, "beta").len(),
        2,
        "after ::reindex the imported rows must be searchable"
    );
}

/// **The equivalence that is the whole correctness argument.** A rebuilt index
/// must be indistinguishable from one that was maintained incrementally — same
/// hits *and the same BM25 scores*, which means the corpus statistics
/// (`total_tokens`, `n_docs` ⇒ `avgdl`) were restored too, not just the postings.
///
/// A rebuild that forgot to reset the doc-stats counter would double-count the
/// corpus and fail here on the scores while passing on the hits — which is
/// exactly the failure this asserts against.
#[test]
fn rebuilt_index_is_equivalent_to_a_fresh_one() {
    const FILLER: &str = "pad pad pad pad pad pad pad pad pad pad";
    let dir = tempfile::tempdir().unwrap();

    let rows = format!(
        r"['a', 'gamma'], ['b', 'gamma delta {FILLER}'], ['c', 'delta'], ['d', 'gamma delta gamma']"
    );

    // Rebuilt: index exists, rows go in, then ::reindex.
    let rebuilt = new_db(&dir.path().join("rebuilt.db"));
    create_doc(&rebuilt);
    create_fts(&rebuilt);
    run(
        &rebuilt,
        &format!(r"?[k, body] <- [{rows}] :put doc {{k => body}}"),
    );
    run(&rebuilt, r"::reindex doc");

    // Fresh: the same rows, indexed incrementally, never rebuilt.
    let fresh = new_db(&dir.path().join("fresh.db"));
    create_doc(&fresh);
    create_fts(&fresh);
    run(
        &fresh,
        &format!(r"?[k, body] <- [{rows}] :put doc {{k => body}}"),
    );

    for q in ["gamma", "delta"] {
        let a = fts_scores(&rebuilt, q);
        let b = fts_scores(&fresh, q);
        assert_eq!(
            a.len(),
            b.len(),
            "hit count differs for '{q}': rebuilt {a:?} vs fresh {b:?}"
        );
        for ((ka, sa), (kb, sb)) in a.iter().zip(&b) {
            assert_eq!(ka, kb, "different documents matched '{q}'");
            assert!(
                (sa - sb).abs() < 1e-9,
                "BM25 score for '{ka}' on '{q}' differs after a rebuild: {sa} vs {sb} — \
                 the corpus statistics (avgdl) were not restored correctly"
            );
        }
    }
}

/// Rebuilding twice must change nothing. A rebuild that failed to clear the old
/// rows first would double its postings here (and skew every score).
#[test]
fn reindex_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(&dir.path().join("idem.db"));

    create_doc(&db);
    create_fts(&db);
    run(
        &db,
        r"?[k, body] <- [['a', 'gamma'], ['b', 'gamma delta']] :put doc {k => body}",
    );

    run(&db, r"::reindex doc");
    let once = fts_scores(&db, "gamma");
    run(&db, r"::reindex doc");
    let twice = fts_scores(&db, "gamma");

    assert_eq!(
        once, twice,
        "a second ::reindex changed the index — the rebuild is not idempotent \
         (most likely it did not clear the old rows before re-deriving)"
    );
}

/// HNSW rebuilds too, and the rebuilt graph answers vector queries.
#[test]
fn reindex_rebuilds_hnsw() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(&dir.path().join("hnsw.db"));

    run(&db, r":create item {k: String => v: <F32; 2>}");
    run(
        &db,
        r"::hnsw create item:vec { dim: 2, m: 8, dtype: F32, fields: [v], distance: L2, ef_construction: 20 }",
    );

    db.import_relations(BTreeMap::from([(
        "item".to_string(),
        NamedRows::new(
            vec!["k".to_string(), "v".to_string()],
            vec![
                vec![
                    cozo::DataValue::from("near"),
                    cozo::DataValue::List(vec![
                        cozo::DataValue::from(1.0),
                        cozo::DataValue::from(0.0),
                    ]),
                ],
                vec![
                    cozo::DataValue::from("far"),
                    cozo::DataValue::List(vec![
                        cozo::DataValue::from(0.0),
                        cozo::DataValue::from(1.0),
                    ]),
                ],
            ],
        ),
    )]))
    .unwrap();

    let probe = r"?[k, d] := ~item:vec{k | query: vec([1.0, 0.0]), k: 2, ef: 20, bind_distance: d} :order d";
    assert!(
        run(&db, probe).rows.is_empty(),
        "precondition: bulk-imported vectors are invisible to HNSW"
    );

    let report = run(&db, r"::reindex item");
    assert_eq!(report.rows[0][1].get_str().unwrap(), "hnsw");

    let hits = run(&db, probe);
    assert_eq!(
        hits.rows.len(),
        2,
        "the rebuilt HNSW must find both vectors"
    );
    assert_eq!(
        hits.rows[0][0].get_str().unwrap(),
        "near",
        "the nearest vector must rank first — the rebuilt graph is wrong"
    );
}

/// LSH rebuilds too — and this is the index that forced the whole design.
///
/// `MinHashLshIndexManifest` stores the *derived* band geometry (`n_bands`,
/// `n_rows_in_band`, `perms`) but **not** the `false_positive_weight` /
/// `false_negative_weight` that produced it. A drop-and-recreate `::reindex`
/// would therefore have to reconstruct a config from defaults and would silently
/// hand back an index with a different recall/precision profile. Rebuilding in
/// place against the stored manifest is what keeps the geometry — so the rebuilt
/// index must behave exactly like the one that was there before.
#[test]
fn reindex_rebuilds_lsh_and_preserves_its_geometry() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(&dir.path().join("lsh.db"));

    create_doc(&db);
    // A deliberately NON-default threshold: if the rebuild reconstructed a config
    // from defaults, the band geometry would change and near-duplicate behaviour
    // with it.
    run(
        &db,
        r"::lsh create doc:lsh {
            extractor: body, tokenizer: Simple, filters: [Lowercase],
            n_perm: 64, target_threshold: 0.4, n_gram: 3
        }",
    );

    let text = "the quick brown fox jumps over the lazy dog";
    let near_dup = |db: &DbInstance, q: &str| -> usize {
        run(
            db,
            &format!(r"?[k] := ~doc:lsh{{k, body | query: '{q}', k: 50}}"),
        )
        .rows
        .len()
    };

    // Put a row the normal way (index maintained), and bulk-import another
    // (index NOT maintained) — so the index is both live and stale.
    run(
        &db,
        &format!(r"?[k, body] <- [['live', '{text}']] :put doc {{k => body}}"),
    );
    db.import_relations(BTreeMap::from([(
        "doc".to_string(),
        NamedRows::new(
            vec!["k".to_string(), "body".to_string()],
            vec![vec![
                cozo::DataValue::from("stranded"),
                cozo::DataValue::from(text),
            ]],
        ),
    )]))
    .unwrap();

    assert_eq!(
        near_dup(&db, text),
        1,
        "precondition: only the :put row is in the index; the imported one is stranded"
    );

    let report = run(&db, r"::reindex doc");
    assert_eq!(report.rows[0][1].get_str().unwrap(), "lsh");

    assert_eq!(
        near_dup(&db, text),
        2,
        "after ::reindex BOTH near-duplicates must be found — the rebuild missed the \
         bulk-imported row"
    );

    // The rebuilt index must still recognise a near-duplicate at the threshold the
    // relation was created with. A rebuild that recomputed the band geometry from
    // default weights could change this answer.
    let one_word_off = "the quick brown fox jumps over the lazy cat";
    assert_eq!(
        near_dup(&db, one_word_off),
        2,
        "a near-duplicate must still be found at the configured threshold — the \
         rebuilt index's band geometry differs from the one that was stored"
    );
}

/// A relation with nothing to rebuild is a loud no-op, not an error: `::reindex`
/// stays scriptable across a set of relations without the caller having to know
/// which ones carry search indexes.
#[test]
fn reindex_without_search_indexes_is_a_loud_noop() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(&dir.path().join("noop.db"));

    create_doc(&db);
    run(&db, r"?[k, body] <- [['k1', 'hello']] :put doc {k => body}");

    let res = run(&db, r"::reindex doc");
    assert_eq!(res.headers[0], "status");
    assert!(
        res.rows[0][0]
            .get_str()
            .unwrap()
            .contains("nothing to rebuild"),
        "expected an explanatory status row, got {:?}",
        res.rows
    );
}

/// A relation that does not exist is an error, not a silent success.
#[test]
fn reindex_of_a_missing_relation_errors() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_db(&dir.path().join("missing.db"));
    assert!(db
        .run_script(
            r"::reindex nope",
            BTreeMap::new(),
            ScriptMutability::Mutable
        )
        .is_err());
}
