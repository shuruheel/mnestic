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
        codes(&w)
            .iter()
            .any(|c| c == "fixed_rule.pagerank.unconverged"),
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

#[test]
fn missing_index_surfaced_in_explain_and_warnings_and_clears_when_indexed() {
    let (_d, db) = db();
    run(&db, ":create edge { src: String, dst: String }");
    run(
        &db,
        "?[src, dst] <- [['e1', 'e2'], ['e3', 'e2']] :put edge { src, dst }",
    );
    run(&db, "::warnings clear");

    // Binding the non-leading key column `dst` without a reverse index forces a
    // full-relation scan (`stored_mat_join`).
    let q = "?[src] := dst = 'e2', *edge{ src, dst }";
    let plan = run(&db, &format!("::explain {{ {q} }}"));
    let has_advisory = plan.rows.iter().any(|row| {
        row[4].get_str() == Some("missing_index_advisory")
            && row[7]
                .get_str()
                .is_some_and(|s| s.contains("edge") && s.contains("dst") && s.contains("::index create"))
    });
    assert!(
        has_advisory,
        "expected missing_index_advisory in ::explain, got: {:?}",
        plan.rows
    );

    // Executing the query records `query.missing_index` in `::warnings`.
    let rows = run(&db, q);
    assert_eq!(rows.rows.len(), 2);
    let w = run(&db, "::warnings");
    assert!(
        codes(&w).iter().any(|c| c == "query.missing_index"),
        "expected query.missing_index in {w:?}"
    );
    let hint = w
        .rows
        .iter()
        .find(|r| r[1].get_str() == Some("query.missing_index"))
        .unwrap()[3]
        .get_str()
        .unwrap();
    assert!(
        hint.contains("::index create edge:by_dst"),
        "hint should suggest the missing index: {hint}"
    );

    // Creating the reverse index eliminates both the explain advisory and warning.
    run(&db, "::index create edge:by_dst { dst, src }");
    run(&db, "::warnings clear");
    let plan_after = run(&db, &format!("::explain {{ {q} }}"));
    assert!(
        !plan_after
            .rows
            .iter()
            .any(|row| row[4].get_str() == Some("missing_index_advisory")),
        "advisory should disappear once indexed: {:?}",
        plan_after.rows
    );
    let rows_after = run(&db, q);
    assert_eq!(rows_after.rows.len(), 2);
    let w_after = run(&db, "::warnings");
    assert!(
        !codes(&w_after).iter().any(|c| c == "query.missing_index"),
        "warning should not fire once indexed: {w_after:?}"
    );
}

#[test]
fn missing_index_covers_neg_join_and_avoids_broken_hint_on_txtime() {
    let (_d, db) = db();
    run(&db, ":create edge { src: String, dst: String }");
    run(
        &db,
        ":create bt { k: String, vt: Validity, tx: TxTime => v: String }",
    );
    run(&db, "?[src, dst] <- [['a', 'b']] :put edge { src, dst }");
    run(
        &db,
        "?[k, vt, v] <- [['k1', 'ASSERT', 'v1']] :put bt { k, vt => v }",
    );

    // Negated join on unindexed `dst` surfaces advisory + actionable hint.
    let neg_q = "?[x] := x = 'b', not *edge{ dst: x }";
    let neg_plan = run(&db, &format!("::explain {{ {neg_q} }}"));
    assert!(
        neg_plan.rows.iter().any(|r| {
            r[4].get_str() == Some("missing_index_advisory")
                && r[7].get_str().is_some_and(|s| {
                    s.contains("stored_neg_mat_join") && s.contains("::index create edge:by_dst")
                })
        }),
        "expected missing_index_advisory on negated full scan: {:?}",
        neg_plan.rows
    );

    // Bitemporal (`TxTime`) full scan surfaces advisory without suggesting `::index create`.
    let bt_q = "?[k] := v = 'v1', *bt{ k, v @ 'NOW' }";
    let bt_plan = run(&db, &format!("::explain {{ {bt_q} }}"));
    let bt_adv = bt_plan
        .rows
        .iter()
        .find(|r| r[4].get_str() == Some("missing_index_advisory"))
        .and_then(|r| r[7].get_str())
        .unwrap_or_else(|| {
            panic!(
                "expected missing_index_advisory for TxTime scan: {:?}",
                bt_plan.rows
            )
        });
    assert!(
        !bt_adv.contains("::index create"),
        "TxTime advisory must not suggest unsupported ::index create: {bt_adv}"
    );
}
