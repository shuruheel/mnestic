/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file, You can obtain one at
 * https://mozilla.org/MPL/2.0/.
 */

//! EA4 Batch 4.1 acceptance tests (`docs/specs/txtime-search-indexes.md` Batches 2 & 3):
//! - Bounded exact `:as_of` vector search on tt-only relations
//! - Current-belief B-tree secondary indexes (`::index create/drop`) on tt-only relations
//!   and planner D6 (current reads use index; historical reads bypass index & stay bounded).

use std::collections::BTreeMap;

use cozo::{DataValue, DbInstance, NamedRows, ScriptMutability};
use tempfile::tempdir;

fn engines(mut test: impl FnMut(&DbInstance)) {
    let dir = tempdir().unwrap();
    #[allow(unused_mut)]
    let mut list = vec!["mem"];
    #[cfg(feature = "storage-sqlite")]
    list.push("sqlite");
    #[cfg(feature = "storage-rocksdb")]
    list.push("rocksdb");
    for engine in list {
        let path = dir.path().join(engine);
        let db = DbInstance::new(engine, path.to_str().unwrap(), "{}").unwrap();
        test(&db);
    }
}

fn run(db: &DbInstance, script: &str) -> NamedRows {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("script failed:\n{script}\nerror: {e:?}"))
}

fn run_ro(db: &DbInstance, script: &str) -> NamedRows {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Immutable)
        .unwrap_or_else(|e| panic!("ro script failed:\n{script}\nerror: {e:?}"))
}

fn max_tt(db: &DbInstance, rel: &str) -> i64 {
    let rows = run_ro(db, &format!("?[max(t)] := *{rel}{{tt}}, t = to_int(tt)")).rows;
    rows[0][0].get_int().unwrap()
}

fn explain_text(db: &DbInstance, query: &str) -> String {
    let rows = run_ro(db, &format!("::explain {{ {query} }}")).rows;
    rows.into_iter()
        .map(|r| {
            r.into_iter()
                .map(|v| format!("{v:?}"))
                .collect::<Vec<_>>()
                .join(" | ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn as_of_vector_search_returns_exact_historical_beliefs_even_after_full_retraction() {
    engines(|db| {
        run(
            db,
            ":create item {id: String, tt: TxTime => tag: String, emb: <F32; 2>}",
        );
        run(
            db,
            "::hnsw create item:vec {dim: 2, m: 16, dtype: F32, fields: [emb], distance: L2, ef_construction: 32, filter: tag != 'skip'}",
        );

        // T1: 'a' is close to [1, 0]; 'b' is close to [0, 1]; 'c' is filtered out by index_filter.
        run(
            db,
            "?[id, tag, emb] <- [['a', 'keep', vec([1.0, 0.0])], ['b', 'keep', vec([0.0, 1.0])], ['c', 'skip', vec([1.0, 0.0])]] :put item {id => tag, emb}",
        );
        let t1 = max_tt(db, "item");

        // T2: move 'a' away and move 'b' right onto [1, 0].
        run(
            db,
            "?[id, tag, emb] <- [['a', 'keep', vec([0.0, 10.0])], ['b', 'keep', vec([1.0, 0.0])]] :put item {id => tag, emb}",
        );
        let t2 = max_tt(db, "item");

        // T3: retract ALL rows — current HNSW graph is now completely empty!
        run(db, "?[id] <- [['a'], ['b'], ['c']] :rm item {id}");

        // Current search returns empty.
        let cur = run_ro(
            db,
            "?[id] := ~item:vec{id | query: vec([1.0, 0.0]), k: 5, ef: 20}",
        );
        assert!(cur.rows.is_empty());

        // :as_of T1 returns 'a' first, 'b' second; 'c' excluded by index_filter.
        let q1 = format!(
            "?[id, dist, f, fi, v, tt] := ~item:vec{{id, tt | query: vec([1.0, 0.0]), k: 5, ef: 20, bind_distance: dist, bind_field: f, bind_field_idx: fi, bind_vector: v}} :order dist :as_of {t1}"
        );
        let explain_vec = explain_text(db, &q1);
        assert!(
            explain_vec.contains("hnsw_history_scan"),
            "expected hnsw_history_scan in explain:\n{explain_vec}"
        );
        let r1 = run_ro(db, &q1);
        let ids1: Vec<&str> = r1.rows.iter().map(|r| r[0].get_str().unwrap()).collect();
        assert_eq!(ids1, vec!["a", "b"]);
        assert_eq!(r1.rows[0][2], DataValue::from("emb"));
        assert_eq!(r1.rows[0][3], DataValue::Null);
        assert!(matches!(r1.rows[0][4], DataValue::Vec(_)));
        assert!(matches!(r1.rows[0][5], DataValue::Validity(_)));

        // :as_of T2 returns 'b' first, 'a' second; radius filter works too.
        let r2 = run_ro(
            db,
            &format!(
                "?[id, dist] := ~item:vec{{id | query: vec([1.0, 0.0]), k: 5, ef: 20, radius: 0.5, bind_distance: dist}} :order dist :as_of {t2}"
            ),
        );
        let ids2: Vec<&str> = r2.rows.iter().map(|r| r[0].get_str().unwrap()).collect();
        assert_eq!(ids2, vec!["b"]);

        // Query-level filter works under :as_of.
        let r2_filtered = run_ro(
            db,
            &format!(
                "?[id] := ~item:vec{{id, tag | query: vec([1.0, 0.0]), k: 5, ef: 20, filter: id == 'a'}} :as_of {t2}"
            ),
        );
        assert_eq!(r2_filtered.rows, vec![vec![DataValue::from("a")]]);

        // GC past T1: :as_of T1 must fail with eval::txtime_below_gc_floor.
        run(db, &format!("::history_gc item {t2}"));
        let err = db
            .run_script(
                &format!(
                    "?[id] := ~item:vec{{id | query: vec([1.0, 0.0]), k: 5, ef: 20}} :as_of {t1}"
                ),
                BTreeMap::new(),
                ScriptMutability::Immutable,
            )
            .unwrap_err();
        assert!(
            format!("{err:?}").contains("eval::txtime_below_gc_floor"),
            "expected gc floor error, got: {err:?}"
        );
    });
}

#[test]
fn btree_secondary_index_on_tt_only_tracks_current_belief_and_bypasses_for_history() {
    engines(|db| {
        run(
            db,
            ":create doc {id: String, tt: TxTime => tag: String, body: String}",
        );

        // Populate with corrections and a retraction BEFORE creating the B-tree index.
        run(
            db,
            "?[id, tag, body] <- [['d1', 'alpha', 'v1'], ['d2', 'alpha', 'v1'], ['d3', 'beta', 'v1']] :put doc {id => tag, body}",
        );
        let t1 = max_tt(db, "doc");
        run(
            db,
            "?[id, tag, body] <- [['d1', 'gamma', 'v2']] :put doc {id => tag, body}",
        );
        run(db, "?[id] <- [['d2']] :rm doc {id}");
        let t2 = max_tt(db, "doc");

        // Reject indexing the tt column (D2).
        let tt_col_err = db
            .run_script(
                "::index create doc:bad {tt, id}",
                BTreeMap::new(),
                ScriptMutability::Mutable,
            )
            .unwrap_err();
        assert!(
            format!("{tt_col_err:?}").contains("eval::txtime_index_column"),
            "expected txtime_index_column, got: {tt_col_err:?}"
        );

        // Create B-tree secondary index on `tag`.
        run(db, "::index create doc:by_tag {tag}");

        // Verify `::columns doc:by_tag` omits `tt` (logical keys only).
        let cols = run_ro(db, "::columns doc:by_tag");
        let col_names: Vec<&str> = cols.rows.iter().map(|r| r[0].get_str().unwrap()).collect();
        assert_eq!(col_names, vec!["tag", "id"]);

        // Verify index holds ONLY current asserted beliefs (d1 -> gamma, d3 -> beta; d2 absent).
        let idx_rows = run_ro(db, "?[tag, id] := *doc:by_tag{tag, id} :order tag, id").rows;
        assert_eq!(
            idx_rows,
            vec![
                vec![DataValue::from("beta"), DataValue::from("d3")],
                vec![DataValue::from("gamma"), DataValue::from("d1")],
            ]
        );

        // Current read uses the secondary index (index-only plan).
        let plan_cur_only = explain_text(db, "?[id] := tag = 'gamma', *doc{id, tag}");
        assert!(
            plan_cur_only.contains(":doc:by_tag"),
            "expected current read to use :doc:by_tag:\n{plan_cur_only}"
        );
        assert_eq!(
            run_ro(db, "?[id] := tag = 'gamma', *doc{id, tag}").rows,
            vec![vec![DataValue::from("d1")]]
        );

        // Current read binding non-indexed column AND `tt` joins index back to base and resolves `tt`.
        let plan_cur_join = explain_text(
            db,
            "?[id, body, tt] := tag = 'gamma', *doc{id, tag, body, tt}",
        );
        assert!(
            plan_cur_join.contains(":doc:by_tag"),
            "expected current read with tt to use :doc:by_tag:\n{plan_cur_join}"
        );
        let join_rows = run_ro(
            db,
            "?[id, body, tt] := tag = 'gamma', *doc{id, tag, body, tt}",
        )
        .rows;
        assert_eq!(join_rows.len(), 1);
        assert_eq!(join_rows[0][0], DataValue::from("d1"));
        assert_eq!(join_rows[0][1], DataValue::from("v2"));
        assert!(matches!(join_rows[0][2], DataValue::Validity(_)));

        // Historical read (:as_of T1) MUST bypass the secondary index and return 'd1' and 'd2' for tag='alpha'.
        let hist_q = format!("?[id] := tag = 'alpha', *doc{{id, tag}} :order id :as_of {t1}");
        let plan_hist = explain_text(db, &hist_q);
        assert!(
            !plan_hist.contains(":doc:by_tag"),
            "historical read must not use current-belief index:\n{plan_hist}"
        );
        assert!(
            plan_hist.contains("history"),
            "historical read explain should show history scan:\n{plan_hist}"
        );
        assert_eq!(
            run_ro(db, &hist_q).rows,
            vec![vec![DataValue::from("d1")], vec![DataValue::from("d2")]]
        );

        // Explicit `@ (tt: T1)` selector also bypasses the index.
        let explicit_q = format!("?[id] := tag = 'alpha', *doc{{id, tag @ (tt: {t1})}} :order id");
        let plan_explicit = explain_text(db, &explicit_q);
        assert!(
            !plan_explicit.contains(":doc:by_tag"),
            "explicit historical read must not use current-belief index:\n{plan_explicit}"
        );
        assert_eq!(
            run_ro(db, &explicit_q).rows,
            vec![vec![DataValue::from("d1")], vec![DataValue::from("d2")]]
        );

        // Negation uses index for current belief and base history for :as_of.
        let neg_cur = run_ro(
            db,
            "q[tag] <- [['alpha'], ['gamma']] ?[tag] := q[tag], not *doc{tag}",
        );
        assert_eq!(neg_cur.rows, vec![vec![DataValue::from("alpha")]]);
        let neg_hist = run_ro(
            db,
            &format!(
                "q[tag] <- [['alpha'], ['gamma']] ?[tag] := q[tag], not *doc{{tag}} :as_of {t1}"
            ),
        );
        assert_eq!(neg_hist.rows, vec![vec![DataValue::from("gamma")]]);

        // Direct index atom and fixed-rule input naming index under :as_of raise search::as_of_unsupported.
        for q in [
            format!("?[id] := *doc:by_tag{{tag: 'gamma', id}} :as_of {t2}"),
            format!("?[rank, id] <~ ReorderSort(*doc:by_tag[tag, id], out: [id]) :as_of {t2}"),
        ] {
            let err = db
                .run_script(&q, BTreeMap::new(), ScriptMutability::Immutable)
                .unwrap_err();
            assert!(
                format!("{err:?}").contains("search::as_of_unsupported"),
                "{q}: {err:?}"
            );
        }

        // Mutations after index creation: update non-indexed field, update indexed field,
        // retract, re-assert, :reconcile, ::evict.
        run(
            db,
            "?[id, tag, body] <- [['d1', 'gamma', 'v3']] :put doc {id => tag, body}",
        );
        assert_eq!(
            run_ro(db, "?[id, body] := tag = 'gamma', *doc{id, tag, body}").rows,
            vec![vec![DataValue::from("d1"), DataValue::from("v3")]]
        );
        run(
            db,
            "?[id, tag, body] <- [['d1', 'delta', 'v4'], ['d2', 'alpha', 'reasserted']] :put doc {id => tag, body}",
        );
        assert_eq!(
            run_ro(db, "?[tag, id] := *doc:by_tag{tag, id} :order tag, id").rows,
            vec![
                vec![DataValue::from("alpha"), DataValue::from("d2")],
                vec![DataValue::from("beta"), DataValue::from("d3")],
                vec![DataValue::from("delta"), DataValue::from("d1")],
            ]
        );
        run(
            db,
            "?[id, tag, body] <- [['d2', 'omega', 'reconciled']] :reconcile doc {id => tag, body}",
        );
        assert_eq!(
            run_ro(db, "?[tag, id] := *doc:by_tag{tag, id} :order tag, id").rows,
            vec![vec![DataValue::from("omega"), DataValue::from("d2")]]
        );
        run(db, "::evict doc [['d2']]");
        assert!(run_ro(db, "?[tag, id] := *doc:by_tag{tag, id}")
            .rows
            .is_empty());

        // Rename & drop keep B-tree index consistent.
        run(
            db,
            "?[id, tag, body] <- [['d9', 'zeta', 'live']] :put doc {id => tag, body}",
        );
        run(db, "::rename doc -> doc2");
        assert_eq!(
            run_ro(db, "?[id] := tag = 'zeta', *doc2{id, tag}").rows,
            vec![vec![DataValue::from("d9")]]
        );
        assert_eq!(
            run_ro(db, "?[tag, id] := *doc2:by_tag{tag, id}").rows,
            vec![vec![DataValue::from("zeta"), DataValue::from("d9")]]
        );
        assert!(db
            .run_script(
                "?[tag, id] := *doc:by_tag{tag, id}",
                BTreeMap::new(),
                ScriptMutability::Immutable,
            )
            .is_err());

        // ::reindex on a B-tree-only tt-only relation short-circuits cleanly.
        let reindex_out = run(db, "::reindex doc2");
        assert_eq!(reindex_out.rows.len(), 1);

        // ::repair_corrupt refuses while a B-tree index is attached.
        assert!(db
            .run_script(
                "::repair_corrupt doc2",
                BTreeMap::new(),
                ScriptMutability::Mutable,
            )
            .is_err());

        run(db, "::index drop doc2:by_tag");
        assert_eq!(
            run_ro(db, "?[id] := tag = 'zeta', *doc2{id, tag}").rows,
            vec![vec![DataValue::from("d9")]]
        );
    });
}

#[test]
fn btree_composite_key_import_duplicate_writes_and_evict_limit() {
    engines(|db| {
        run(
            db,
            ":create edge_tt {src: String, dst: String, tt: TxTime => kind: String, weight: Float}",
        );
        run(db, "::index create edge_tt:by_dst_kind {dst, kind}");

        // Duplicate writes to the same logical key in one transaction: last write wins.
        run(
            db,
            "{ ?[src, dst, kind, weight] <- [['a', 'b', 'old', 1.0]] :put edge_tt {src, dst => kind, weight} }\n\
             { ?[src, dst, kind, weight] <- [['a', 'b', 'new', 2.0]] :put edge_tt {src, dst => kind, weight} }",
        );
        assert_eq!(
            run_ro(
                db,
                "?[dst, kind, src] := *edge_tt:by_dst_kind{dst, kind, src}"
            )
            .rows,
            vec![vec![
                DataValue::from("b"),
                DataValue::from("new"),
                DataValue::from("a"),
            ]]
        );

        // Index-with-join on composite logical key (non-leading key `dst` + value `kind`, fetching `weight` and `tt`).
        let plan = explain_text(
            db,
            "?[src, weight, tt] := dst = 'b', kind = 'new', *edge_tt{src, dst, kind, weight, tt}",
        );
        assert!(plan.contains(":edge_tt:by_dst_kind"), "plan:\n{plan}");
        let rows = run_ro(
            db,
            "?[src, weight, tt] := dst = 'b', kind = 'new', *edge_tt{src, dst, kind, weight, tt}",
        )
        .rows;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], DataValue::from("a"));
        assert_eq!(rows[0][1], DataValue::from(2.0));
        assert!(matches!(rows[0][2], DataValue::Validity(_)));

        // import_relations maintains the B-tree index.
        let imported = NamedRows::new(
            vec!["src".into(), "dst".into(), "kind".into(), "weight".into()],
            vec![vec![
                DataValue::from("x"),
                DataValue::from("y"),
                DataValue::from("imp"),
                DataValue::from(3.5),
            ]],
        );
        db.import_relations(BTreeMap::from([("edge_tt".to_string(), imported)]))
            .unwrap();
        assert_eq!(
            run_ro(
                db,
                "?[src] := dst = 'y', kind = 'imp', *edge_tt{src, dst, kind}"
            )
            .rows,
            vec![vec![DataValue::from("x")]]
        );

        // Evicting > 1,000 keys on a B-tree-indexed tt-only relation raises eval::evict_batch_limit.
        let keys_literal: String = (0..1001)
            .map(|i| format!("['s{i}', 'd{i}']"))
            .collect::<Vec<_>>()
            .join(", ");
        let evict_err = db
            .run_script(
                &format!("::evict edge_tt [{keys_literal}]"),
                BTreeMap::new(),
                ScriptMutability::Mutable,
            )
            .unwrap_err();
        assert!(
            format!("{evict_err:?}").contains("eval::evict_batch_limit"),
            "expected evict_batch_limit, got: {evict_err:?}"
        );
    });
}

#[test]
fn bitemporal_relation_still_rejects_btree_index() {
    engines(|db| {
        run(
            db,
            ":create bi {id: String, vt: Validity, tt: TxTime => tag: String}",
        );
        let err = db
            .run_script(
                "::index create bi:by_tag {tag}",
                BTreeMap::new(),
                ScriptMutability::Mutable,
            )
            .unwrap_err();
        assert!(
            format!("{err:?}").contains("eval::txtime_unsupported_op"),
            "expected bitemporal rejection, got: {err:?}"
        );
    });
}

#[test]
fn imperative_btree_index_create_on_tt_relation_is_rejected() {
    engines(|db| {
        run(db, ":create doc {id: String, tt: TxTime => tag: String}");
        let err = db
            .run_script(
                "{ ::index create doc:by_tag {tag} }",
                BTreeMap::new(),
                ScriptMutability::Mutable,
            )
            .unwrap_err();
        assert!(
            format!("{err:?}").contains("eval::txtime_ddl_standalone"),
            "expected standalone ddl error, got: {err:?}"
        );
    });
}
