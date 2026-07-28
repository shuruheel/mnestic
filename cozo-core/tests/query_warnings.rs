/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Structured query diagnostics (`::warnings`, `Db::recent_warnings`): the
//! typed surface for what previously reached only `log::warn!`. sqlite
//! backend per the repo test-backend rule (the Cartesian warning comes from
//! the planner).

use cozo::{DataValue, DbInstance, ScriptMutability};
use std::collections::BTreeMap;

fn db() -> (tempfile::TempDir, DbInstance) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("warnings.db");
    let db = DbInstance::new("sqlite", path.to_str().unwrap(), Default::default()).unwrap();
    (dir, db)
}

fn run(db: &DbInstance, script: &str) -> cozo::NamedRows {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("script failed: {script}\n{e:?}"))
}

fn codes(rows: &cozo::NamedRows) -> Vec<String> {
    rows.rows
        .iter()
        .map(|r| r[1].get_str().unwrap().to_string())
        .collect()
}

#[test]
fn cartesian_step_warning_is_structured() {
    let (_d, db) = db();
    run(&db, ":create r1 {a: Int}");
    run(&db, ":create r2 {b: Int}");
    run(&db, ":create r3 {c: Int}");
    run(&db, "?[a] <- [[1], [2]] :put r1 {a}");
    run(&db, "?[b] <- [[3]] :put r2 {b}");
    run(&db, "?[c] <- [[4]] :put r3 {c}");
    // Disconnected conjunction: no shared variables — a Cartesian product the
    // greedy reorder cannot fix. (Three stored atoms: the reorder pass only
    // engages at >= 3, so a 2-atom Cartesian is below its horizon.)
    run(&db, "?[a, b, c] := *r1[a], *r2[b], *r3[c]");
    let w = run(&db, "::warnings");
    assert_eq!(
        w.headers,
        vec!["seq", "code", "message", "hint"],
        "stable column contract"
    );
    assert!(
        codes(&w).iter().any(|c| c == "query.cartesian_step"),
        "expected query.cartesian_step in {w:?}"
    );
    let hint = w.rows.last().unwrap()[3].get_str().unwrap();
    assert!(hint.contains("shared variable"), "actionable hint: {hint}");

    // clear empties the ring.
    run(&db, "::warnings clear");
    let w = run(&db, "::warnings");
    assert!(w.rows.is_empty());
}

#[test]
fn seq_is_monotone_across_queries() {
    let (_d, db) = db();
    run(&db, ":create s1 {a: Int}");
    run(&db, ":create s2 {b: Int}");
    run(&db, ":create s3 {c: Int}");
    run(&db, "?[a] <- [[1]] :put s1 {a}");
    run(&db, "?[b] <- [[1]] :put s2 {b}");
    run(&db, "?[c] <- [[1]] :put s3 {c}");
    run(&db, "?[a, b, c] := *s1[a], *s2[b], *s3[c]");
    run(&db, "?[a, b, c] := *s1[a], *s2[b], *s3[c]");
    let w = run(&db, "::warnings");
    let seqs: Vec<i64> = w.rows.iter().map(|r| r[0].get_int().unwrap()).collect();
    assert!(seqs.len() >= 2);
    assert!(seqs.windows(2).all(|p| p[0] < p[1]), "monotone: {seqs:?}");
}

#[test]
fn pagerank_unconverged_warning() {
    let (_d, db) = db();
    run(&db, ":create e {f: Int, t: Int}");
    run(
        &db,
        "?[f, t] <- [[1,2],[2,3],[3,4],[4,5],[5,1],[1,3],[2,4]] :put e {f, t}",
    );
    run(
        &db,
        "?[node, rank] <~ PageRank(*e[], iterations: 1, epsilon: 0.0000001)",
    );
    let w = run(&db, "::warnings");
    assert!(
        codes(&w).iter().any(|c| c == "fixed_rule.pagerank.unconverged"),
        "expected pagerank warning in {w:?}"
    );
}

#[test]
fn import_stranded_indexes_warning() {
    let (_d, db) = db();
    run(&db, ":create doc {id: Int => body: String}");
    run(
        &db,
        "::fts create doc:idx { extractor: body, tokenizer: Simple, filters: [Lowercase] }",
    );
    let mut to_import = BTreeMap::new();
    to_import.insert(
        "doc".to_string(),
        cozo::NamedRows::new(
            vec!["id".to_string(), "body".to_string()],
            vec![vec![DataValue::from(1), DataValue::from("hello world")]],
        ),
    );
    db.import_relations(to_import).unwrap();
    let w = run(&db, "::warnings");
    assert!(
        codes(&w).iter().any(|c| c == "import.stranded_indexes"),
        "expected stranded-index warning in {w:?}"
    );
    let hint = w.rows.last().unwrap()[3].get_str().unwrap();
    assert!(hint.contains("::reindex doc"), "hint names the fix: {hint}");
}

#[test]
fn rust_accessor_matches_sysop() {
    let (_d, db) = db();
    run(&db, ":create t1 {a: Int}");
    run(&db, ":create t2 {b: Int}");
    run(&db, ":create t3 {c: Int}");
    run(&db, "?[a] <- [[1]] :put t1 {a}");
    run(&db, "?[b] <- [[1]] :put t2 {b}");
    run(&db, "?[c] <- [[1]] :put t3 {c}");
    run(&db, "?[a, b, c] := *t1[a], *t2[b], *t3[c]");
    let via_sysop = run(&db, "::warnings");
    let via_rust = match &db {
        DbInstance::Sqlite(inner) => inner.recent_warnings(),
        _ => unreachable!(),
    };
    assert_eq!(via_sysop.rows.len(), via_rust.len());
    assert_eq!(
        via_rust.last().unwrap().1.code,
        via_sysop.rows.last().unwrap()[1].get_str().unwrap()
    );
}
