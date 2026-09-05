/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Query-side incomplete-term normalization; docs/specs/fts-prefix.md.
use cozo::{DataValue, DbInstance, NamedRows, ScriptMutability};
use std::collections::BTreeMap;

fn run(db: &DbInstance, script: &str) -> NamedRows {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("{script}: {e:?}"))
}

fn setup(tokenizer: &str, filters: &str, rows: &str) -> (tempfile::TempDir, DbInstance) {
    let dir = tempfile::tempdir().unwrap();
    let db = reopen(&dir);
    let filters = if filters.is_empty() {
        String::new()
    } else {
        format!(", filters: [{filters}]")
    };
    run(&db, ":create doc {id: Int => body: String}");
    run(
        &db,
        &format!("::fts create doc:idx {{extractor: body, tokenizer: {tokenizer}{filters}}}"),
    );
    run(
        &db,
        &format!("?[id, body] <- [{rows}] :put doc {{id => body}}"),
    );
    (dir, db)
}

fn reopen(dir: &tempfile::TempDir) -> DbInstance {
    DbInstance::new(
        "sqlite",
        dir.path().join("prefix.db").to_str().unwrap(),
        Default::default(),
    )
    .unwrap()
}

fn search(db: &DbInstance, q: &str) -> cozo::NamedRows {
    db.run_script(
        "?[id, score] := ~doc:idx{id | query: $q, k: 100, bind_score: score} :order id",
        BTreeMap::from([("q".into(), DataValue::from(q))]),
        ScriptMutability::Immutable,
    )
    .unwrap_or_else(|e| panic!("query {q:?}: {e:?}"))
}

fn ids(db: &DbInstance, q: &str) -> Vec<i64> {
    search(db, q)
        .rows
        .iter()
        .map(|r| r[0].get_int().unwrap())
        .collect()
}

#[test]
fn lowercase_prefixes_share_exact_normalization_and_composition() {
    let (_dir, db) = setup(
        "Simple",
        "Lowercase",
        "[1, 'Diwank fox'], [2, 'Charlie'], [3, 'ДИВАН'], [4, 'İstanbul'], [5, 'ΟΣΜΗ']",
    );
    for q in [
        "di*",
        "Di*",
        "DI*",
        "\"DI\"*",
        "NEAR(DI* fox)",
        "DI* AND fox",
    ] {
        assert_eq!(ids(&db, q), vec![1], "{q}");
    }
    assert_eq!(ids(&db, "Diwank"), vec![1]);
    assert_eq!(ids(&db, "ДИ*"), vec![3]);
    assert_eq!(ids(&db, "İS*"), vec![4]);
    assert_eq!(ids(&db, "ΟΣ*"), vec![5]);
    assert_eq!(ids(&db, "DI* OR CHA*"), vec![1, 2]);
    assert_eq!(ids(&db, "DI* NOT fox"), Vec::<i64>::new());
    let plain = search(&db, "Di*").rows[0][1].get_float().unwrap();
    let boosted = search(&db, "DI*^3").rows[0][1].get_float().unwrap();
    assert_eq!(search(&db, "DI*^3").rows, search(&db, "DI*^3.0").rows);
    assert_eq!(search(&db, "fox^2").rows, search(&db, "fox^2.0").rows);
    assert!((boosted - 3.0 * plain).abs() < 1e-10);
}

#[test]
fn folding_respects_configured_order_and_no_filter_is_case_sensitive() {
    for filters in ["AsciiFolding, Lowercase", "LowerCase, AsciiFolding"] {
        let (_dir, db) = setup(
            "Simple",
            filters,
            "[1, 'Éléphant'], [2, 'Æther'], [3, 'Straße']",
        );
        for (q, want) in [
            ("ÉL*", 1),
            ("el*", 1),
            ("ÆT*", 2),
            ("aet*", 2),
            ("STRASS*", 3),
        ] {
            assert_eq!(ids(&db, q), vec![want], "{filters}: {q}");
        }
    }
    // Small-capital A lowercases to itself but folds to capital A: order matters.
    let (_dir, db) = setup("Simple", "Lowercase, AsciiFolding", "[1, 'ᴀcorn']");
    assert_eq!(ids(&db, "ᴀ*"), vec![1]);
    assert!(ids(&db, "a*").is_empty());
    let (_dir, db) = setup("Simple", "AsciiFolding, Lowercase", "[1, 'ᴀcorn']");
    assert_eq!(ids(&db, "ᴀ*"), vec![1]);
    assert_eq!(ids(&db, "a*"), vec![1]);
    let (_dir, db) = setup("Simple", "", "[1, 'Diwank']");
    assert_eq!(ids(&db, "Di*"), vec![1]);
    assert!(ids(&db, "di*").is_empty());
}

#[test]
fn incomplete_terms_skip_full_word_filters() {
    let (_dir, db) = setup(
        "Simple",
        "Lowercase, Stemmer('english'), Stopwords(['the']), RemoveLong(12)",
        "[1, 'running'], [2, 'theory'], [3, 'relational']",
    );
    assert_eq!(ids(&db, "RUN*"), vec![1]);
    assert!(
        ids(&db, "RUNNING*").is_empty(),
        "do not stem incomplete input to run"
    );
    assert_eq!(ids(&db, "running"), vec![1], "exact terms still stem");
    assert_eq!(
        ids(&db, "THE*"),
        vec![2],
        "do not discard stopword prefixes"
    );
    assert!(ids(&db, "the").is_empty());
    assert!(
        ids(&db, "RELATIONAL*").is_empty(),
        "prefix targets indexed stems"
    );
    assert!(
        ids(&db, "RUN* THISPREFIXISTOOLONG*").is_empty(),
        "a discarded prefix must not drop an AND constraint"
    );
    let (_dir, split) = setup(
        "Simple",
        "Lowercase, SplitCompoundWords(['rain', 'bow'])",
        "[1, 'rainbow']",
    );
    assert_eq!(ids(&split, "RAIN*"), vec![1]);
    assert!(
        ids(&split, "RAINB*").is_empty(),
        "do not split an incomplete compound"
    );
}

#[test]
fn raw_and_ngram_indexes_receive_one_prefix() {
    let (_dir, raw) = setup("Raw", "Lowercase, AlphaNumOnly", "[1, 'Diwank']");
    assert_eq!(ids(&raw, "DI*"), vec![1]);
    let (_dir, grams) = setup("NGram(2, 3)", "Lowercase", "[1, 'ABCD'], [2, 'ABXY BCD']");
    assert_eq!(
        ids(&grams, "ABC*"),
        vec![1],
        "do not split ABC into AB AND BC"
    );
    assert!(
        ids(&grams, "ABCD*").is_empty(),
        "no indexed term has four characters"
    );
}

#[test]
fn unsupported_or_empty_prefixes_never_broaden_search() {
    let (_dir, db) = setup(
        "Simple",
        "Lowercase, Stopwords(['the'])",
        "[1, 'the fox'], [2, 'theory']",
    );
    for q in ["\"the fox\"*", "NEAR(\"the fox\"* theory)"] {
        let error = db
            .run_script(
                "?[id] := ~doc:idx{id | query: $q, k: 10}",
                BTreeMap::from([("q".into(), DataValue::from(q))]),
                ScriptMutability::Immutable,
            )
            .unwrap_err();
        assert_eq!(
            error.code().unwrap().to_string(),
            "parser::fts::phrase_prefix_unsupported",
            "{q}: {error:?}"
        );
    }
    for q in ["*fox", "*", "fox OR *the", "fox^9223372036854775808"] {
        assert!(
            db.run_script(
                "?[id] := ~doc:idx{id | query: $q, k: 10}",
                BTreeMap::from([("q".into(), DataValue::from(q))]),
                ScriptMutability::Immutable
            )
            .is_err(),
            "{q}"
        );
    }
    assert!(ids(&db, "\"\"*").is_empty());
    assert!(ids(&db, "\" \"*").is_empty());
}

#[test]
fn prefix_candidates_updates_deletes_and_reopen() {
    let (dir, db) = setup(
        "Simple",
        "Lowercase",
        "[1, 'Diwank'], [2, 'Diana'], [3, 'outside']",
    );
    let scoped = db
        .run_script(
            "?[id] := ~doc:idx{id | query: 'DI*', k: 1, candidates: [2]} :order id",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .unwrap();
    assert_eq!(scoped.rows, vec![vec![DataValue::from(2)]]);
    run(&db, "?[id, body] <- [[1, 'Charlie']] :put doc {id => body}");
    assert_eq!(ids(&db, "DI*"), vec![2]);
    run(&db, "?[id] <- [[2]] :rm doc {id}");
    assert!(ids(&db, "DI*").is_empty());
    drop(db);
    let db = reopen(&dir);
    assert_eq!(ids(&db, "CHA*"), vec![1]);
    assert!(ids(&db, "DI*").is_empty());
}
