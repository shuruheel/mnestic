/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Exact-phrase FTS: contract fixtures per docs/specs/fts-phrase-and-snippets.md §9.
//! sqlite backend per the repo test-backend rule. NB fixture vocabulary must
//! avoid EN stopwords when Stopwords('en') is in play ("over" is one).

use cozo::{DbInstance, ScriptMutability};
use std::collections::BTreeMap;

fn db() -> (tempfile::TempDir, DbInstance) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fts_phrase.db");
    let db = DbInstance::new("sqlite", path.to_str().unwrap(), Default::default()).unwrap();
    (dir, db)
}

fn run(db: &DbInstance, script: &str) -> cozo::NamedRows {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("script failed: {script}\n{e:?}"))
}

fn run_err(db: &DbInstance, script: &str) -> String {
    match db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable) {
        Ok(r) => panic!("expected error, got rows: {r:?}"),
        Err(e) => format!("{e:?}"),
    }
}

fn ids(rows: &cozo::NamedRows) -> Vec<i64> {
    let mut v: Vec<i64> = rows.rows.iter().map(|r| r[0].get_int().unwrap()).collect();
    v.sort();
    v
}

fn setup(db: &DbInstance) {
    run(db, ":create doc {id: Int => body: String}");
    run(
        db,
        "::fts create doc:idx { extractor: body, tokenizer: Simple, filters: [Lowercase] }",
    );
    run(
        db,
        r#"?[id, body] <- [
            [1, "hello world again"],
            [2, "hello wide world"],
            [3, "say hello world hello world bye"]
        ] :put doc {id => body}"#,
    );
}

// The headline: a quoted multi-word query requires adjacency in order.
#[test]
fn phrase_requires_adjacency() {
    let (_d, db) = db();
    setup(&db);
    let res = run(&db, r#"?[id] := ~doc:idx{id | query: '"hello world"', k: 10}"#);
    assert_eq!(ids(&res), vec![1, 3], "non-adjacent doc 2 must be excluded");
}

// Unquoted stays AND-of-terms — byte-identical to pre-0.13.2.
#[test]
fn unquoted_stays_and() {
    let (_d, db) = db();
    setup(&db);
    let res = run(&db, r#"?[id] := ~doc:idx{id | query: 'hello world', k: 10}"#);
    assert_eq!(ids(&res), vec![1, 2, 3]);
}

// Quoted single token ≡ unquoted single token.
#[test]
fn quoted_single_token_equivalent() {
    let (_d, db) = db();
    setup(&db);
    let quoted = run(&db, r#"?[id] := ~doc:idx{id | query: '"world"', k: 10}"#);
    let bare = run(&db, r#"?[id] := ~doc:idx{id | query: 'world', k: 10}"#);
    assert_eq!(ids(&quoted), ids(&bare));
}

// Order matters. Note doc 3 ("say hello world hello world bye") genuinely
// contains "world hello" as an overlapping occurrence (world@2, hello@3) —
// docs 1 and 2, which contain both words but never in that order, are the
// ones the ordering must exclude.
#[test]
fn phrase_is_ordered() {
    let (_d, db) = db();
    setup(&db);
    let res = run(&db, r#"?[id] := ~doc:idx{id | query: '"world hello"', k: 10}"#);
    assert_eq!(ids(&res), vec![3]);
    let res = run(&db, r#"?[id] := ~doc:idx{id | query: '"again hello"', k: 10}"#);
    assert_eq!(ids(&res), Vec::<i64>::new());
}

// tf = anchor count: doc 3 contains the phrase twice and must outrank doc 1
// under Tf scoring.
#[test]
fn phrase_tf_is_anchor_count() {
    let (_d, db) = db();
    setup(&db);
    let res = run(
        &db,
        r#"?[id, s] := ~doc:idx{id | query: '"hello world"', k: 10, score_kind: 'tf', bind_score: s}
           :order -s"#,
    );
    assert_eq!(res.rows[0][0].get_int().unwrap(), 3);
    assert!(res.rows[0][1].get_float().unwrap() > res.rows[1][1].get_float().unwrap());
}

// §4.1: a removed stopword leaves a hole that matches exactly one token —
// any token — symmetrically on both sides.
#[test]
fn stopword_hole_is_one_token_wildcard() {
    let (_d, db) = db();
    run(&db, ":create sdoc {id: Int => body: String}");
    run(
        &db,
        "::fts create sdoc:idx { extractor: body, tokenizer: Simple, filters: [Lowercase, Stopwords('en')] }",
    );
    run(
        &db,
        r#"?[id, body] <- [
            [1, "alpha the beta"],
            [2, "alpha that beta"],
            [3, "alpha beta"],
            [4, "alpha gamma delta beta"]
        ] :put sdoc {id => body}"#,
    );
    // Query "alpha the beta": alpha@0, hole@1, beta@2. Docs 1 and 2 have beta
    // two slots after alpha (the middle token being a stopword or not is
    // irrelevant — the hole constrains nothing). Doc 3 (adjacent) and doc 4
    // (three slots) must NOT match.
    let res = run(
        &db,
        r#"?[id] := ~sdoc:idx{id | query: '"alpha the beta"', k: 10}"#,
    );
    assert_eq!(ids(&res), vec![1, 2]);
}

// §3.2: a quoted phrase that is ALL stopwords tokenizes to nothing — empty
// query, empty result, no error.
#[test]
fn all_stopword_phrase_is_empty() {
    let (_d, db) = db();
    run(&db, ":create s2 {id: Int => body: String}");
    run(
        &db,
        "::fts create s2:idx { extractor: body, tokenizer: Simple, filters: [Lowercase, Stopwords('en')] }",
    );
    run(&db, r#"?[id, body] <- [[1, "alpha beta"]] :put s2 {id => body}"#);
    let res = run(&db, r#"?[id] := ~s2:idx{id | query: '"the of"', k: 10}"#);
    assert_eq!(ids(&res), Vec::<i64>::new());
}

// §3.4: phrase-prefix is a named error (was: silently zero rows).
#[test]
fn phrase_prefix_is_named_error() {
    let (_d, db) = db();
    setup(&db);
    let err = run_err(&db, r#"?[id] := ~doc:idx{id | query: '"hello wor"*', k: 10}"#);
    assert!(
        err.contains("phrase_prefix_unsupported"),
        "wrong error: {err}"
    );
}

// §3.4: a quoted multi-word phrase inside NEAR is a named error (was: bag).
#[test]
fn phrase_in_near_is_named_error() {
    let (_d, db) = db();
    setup(&db);
    let err = run_err(
        &db,
        r#"?[id] := ~doc:idx{id | query: 'NEAR/3("hello world" bye)', k: 10}"#,
    );
    assert!(
        err.contains("phrase_in_near_unsupported"),
        "wrong error: {err}"
    );
}

// A quoted SINGLE token inside NEAR stays legal (pre-0.13.2 behavior).
#[test]
fn single_token_quoted_in_near_still_legal() {
    let (_d, db) = db();
    setup(&db);
    let res = run(
        &db,
        r#"?[id] := ~doc:idx{id | query: 'NEAR/3("hello" bye)', k: 10}"#,
    );
    assert_eq!(ids(&res), vec![3]);
}

// §4.3: phrase against an NGram index is a named error, term search works.
#[test]
fn ngram_phrase_is_named_error_terms_still_work() {
    let (_d, db) = db();
    run(&db, ":create nd {id: Int => body: String}");
    run(
        &db,
        "::fts create nd:idx { extractor: body, tokenizer: NGram(2, 3) }",
    );
    run(&db, r#"?[id, body] <- [[1, "hello world"]] :put nd {id => body}"#);
    let err = run_err(&db, r#"?[id] := ~nd:idx{id | query: '"hel wor"', k: 10}"#);
    assert!(err.contains("phrase_without_positions"), "wrong error: {err}");
    let ok = run(&db, r#"?[id] := ~nd:idx{id | query: 'hel', k: 10}"#);
    assert_eq!(ids(&ok), vec![1]);
}

// §4.2: phrases match stems symmetrically — both sides run the index analyzer.
#[test]
fn stemmed_phrase_matches_stems() {
    let (_d, db) = db();
    run(&db, ":create st {id: Int => body: String}");
    run(
        &db,
        "::fts create st:idx { extractor: body, tokenizer: Simple, filters: [Lowercase, Stemmer('english')] }",
    );
    run(
        &db,
        r#"?[id, body] <- [
            [1, "connections refusing"],
            [2, "refusing connections"]
        ] :put st {id => body}"#,
    );
    let res = run(
        &db,
        r#"?[id] := ~st:idx{id | query: '"connection refused"', k: 10}"#,
    );
    assert_eq!(ids(&res), vec![1], "stems match in order; reversed doc 2 must not");
}

fn spans_of(rows: &cozo::NamedRows, row: usize, col: usize) -> Vec<(i64, i64)> {
    match &rows.rows[row][col] {
        cozo::DataValue::List(l) => l
            .iter()
            .map(|s| match s {
                cozo::DataValue::List(pair) => {
                    (pair[0].get_int().unwrap(), pair[1].get_int().unwrap())
                }
                other => panic!("unexpected span: {other:?}"),
            })
            .collect(),
        other => panic!("unexpected spans value: {other:?}"),
    }
}

// §6.1: bind_spans returns byte offsets of the matched occurrences.
// doc 1 = "hello world again": hello@0..5, world@6..11, again@12..17.
#[test]
fn bind_spans_term_and_phrase() {
    let (_d, db) = db();
    setup(&db);
    // Term: each occurrence's own span.
    let res = run(
        &db,
        r#"?[id, sp] := ~doc:idx{id | query: 'again', k: 10, bind_spans: sp}"#,
    );
    assert_eq!(ids(&res), vec![1]);
    assert_eq!(spans_of(&res, 0, 1), vec![(12, 17)]);
    // Phrase: first token's `from` to last matched token's `to`, per anchor.
    let res = run(
        &db,
        r#"?[id, sp] := ~doc:idx{id | query: '"hello world"', k: 10, bind_spans: sp} :order id"#,
    );
    assert_eq!(ids(&res), vec![1, 3]);
    assert_eq!(spans_of(&res, 0, 1), vec![(0, 11)]);
    // doc 3 = "say hello world hello world bye": two anchors.
    assert_eq!(spans_of(&res, 1, 1), vec![(4, 15), (16, 27)]);
}

// §9: offsets are BYTE offsets — multi-byte text must round-trip correctly.
#[test]
fn bind_spans_multibyte() {
    let (_d, db) = db();
    run(&db, ":create mb {id: Int => body: String}");
    run(
        &db,
        "::fts create mb:idx { extractor: body, tokenizer: Simple, filters: [Lowercase] }",
    );
    // "héllo wörld": héllo = 6 bytes (é is 2), space at 6, wörld = 7..13.
    run(&db, r#"?[id, body] <- [[1, "héllo wörld"]] :put mb {id => body}"#);
    let res = run(
        &db,
        r#"?[id, sp] := ~mb:idx{id | query: '"héllo wörld"', k: 10, bind_spans: sp}"#,
    );
    assert_eq!(ids(&res), vec![1]);
    let spans = spans_of(&res, 0, 1);
    assert_eq!(spans, vec![(0, 13)]);
    // The span must slice the original text cleanly on char boundaries.
    let body = "héllo wörld";
    assert_eq!(&body[spans[0].0 as usize..spans[0].1 as usize], body);
}

// §6.2: snippet(text, spans, window) — end-to-end with bind_spans.
#[test]
fn snippet_end_to_end() {
    let (_d, db) = db();
    run(&db, ":create sn {id: Int => body: String}");
    run(
        &db,
        "::fts create sn:idx { extractor: body, tokenizer: Simple, filters: [Lowercase] }",
    );
    let long = "prelude words that pad the head. the needle phrase sits here in the middle. \
                and a long tail of trailing words follows to force truncation on both sides.";
    run(
        &db,
        &format!(r#"?[id, body] <- [[1, "{long}"]] :put sn {{id => body}}"#),
    );
    let res = run(
        &db,
        r#"?[snip] := ~sn:idx{id, body | query: '"needle phrase"', k: 5, bind_spans: sp},
                     snip = snippet(body, sp, 30)"#,
    );
    let snip = res.rows[0][0].get_str().unwrap();
    assert!(snip.contains("needle phrase"), "window must cover the match: {snip}");
    assert!(snip.starts_with('…') && snip.ends_with('…'), "both ends truncated: {snip}");
    assert!(snip.chars().count() <= 32, "window + 2 ellipses at most: {snip}");

    // Highlight form: markers wrap the matched span.
    let res = run(
        &db,
        r#"?[snip] := ~sn:idx{id, body | query: '"needle phrase"', k: 5, bind_spans: sp},
                     snip = snippet(body, sp, 30, '<b>', '</b>')"#,
    );
    let snip = res.rows[0][0].get_str().unwrap();
    assert!(snip.contains("<b>needle phrase</b>"), "markers must wrap: {snip}");
}

// snippet is a pure function: no-span and multi-byte edges.
#[test]
fn snippet_pure_edges() {
    let (_d, db) = db();
    // No spans: head of the text, truncation marked.
    let res = run(&db, r#"?[s] := s = snippet('abcdefghij', [], 4)"#);
    assert_eq!(res.rows[0][0].get_str().unwrap(), "abcd…");
    // Multi-byte: window counts CHARS and never splits a code point.
    let res = run(
        &db,
        r#"?[s] := s = snippet('ααββγγδδεε', [[4, 8]], 4, '<', '>')"#,
    );
    let s = res.rows[0][0].get_str().unwrap();
    assert!(s.contains("<ββ>"), "span [4,8) is the two-byte chars ββ: {s}");
    // Malformed spans are dropped, not fatal.
    let res = run(
        &db,
        r#"?[s] := s = snippet('hello world', [[6, 5], [-2, 3], [6, 11]], 20)"#,
    );
    assert_eq!(res.rows[0][0].get_str().unwrap(), "hello world");
}

// §7 regression: NEAR must scan each literal ONCE. The pre-0.13.2 bug
// (first literal seeded the intersection and was then re-scanned by the main
// loop) was invisible in results — self-distance 0 always survived — so only
// a scan count can pin the fix. The counter is thread-local and query eval
// runs on this thread, so parallel tests cannot perturb the deltas.
#[test]
fn near_scans_each_literal_once() {
    let (_d, db) = db();
    setup(&db);
    let count = || cozo::FTS_LITERAL_SCANS.with(|c| c.get());
    let before = count();
    run(&db, r#"?[id] := ~doc:idx{id | query: 'NEAR/3(hello world)', k: 10}"#);
    let near_delta = count() - before;
    let before = count();
    run(&db, r#"?[id] := ~doc:idx{id | query: '"hello world"', k: 10}"#);
    let phrase_delta = count() - before;
    assert_eq!(near_delta, 2, "NEAR of 2 literals must scan exactly 2 (was 3 pre-fix)");
    assert_eq!(phrase_delta, 2, "phrase of 2 tokens must scan exactly 2");
}

// §9 subset oracle: on a generated corpus, phrase results ⊆ AND results.
#[test]
fn phrase_subset_of_and() {
    let (_d, db) = db();
    run(&db, ":create g {id: Int => body: String}");
    run(
        &db,
        "::fts create g:idx { extractor: body, tokenizer: Simple, filters: [Lowercase] }",
    );
    // Deterministic corpus cycling a tiny vocabulary.
    let vocab = ["red", "blue", "green", "fish", "bird"];
    let mut puts = String::from("?[id, body] <- [");
    for i in 0..40i64 {
        let w = |k: i64| vocab[((i * 7 + k * 3) % 5) as usize];
        puts.push_str(&format!(
            r#"[{i}, "{} {} {} {}"],"#,
            w(0),
            w(1),
            w(2),
            w(3)
        ));
    }
    puts.push_str("] :put g {id => body}");
    run(&db, &puts);
    for pair in [("red", "blue"), ("blue", "fish"), ("green", "bird")] {
        let phrase = ids(&run(
            &db,
            &format!(r#"?[id] := ~g:idx{{id | query: '"{} {}"', k: 100}}"#, pair.0, pair.1),
        ));
        let and = ids(&run(
            &db,
            &format!(r#"?[id] := ~g:idx{{id | query: '{} {}', k: 100}}"#, pair.0, pair.1),
        ));
        for id in &phrase {
            assert!(and.contains(id), "phrase result {id} missing from AND set");
        }
    }
}
