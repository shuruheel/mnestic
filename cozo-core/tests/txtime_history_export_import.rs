/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use cozo::{
    DataValue, DbInstance, HistoryImportReport, NamedRows, ScriptMutability, TxTimeExportBundle,
    Validity,
};
use serde_json::json;

fn run(db: &DbInstance, script: &str) -> NamedRows {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("script failed:\n{script}\nerror: {e:?}"))
}

fn run_err(db: &DbInstance, script: &str) -> String {
    let err = db
        .run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
        .expect_err(&format!("expected failure for script:\n{script}"));
    format!("{err:?}")
}

fn get_tt_us(v: &DataValue) -> i64 {
    match v {
        DataValue::Validity(vld) => vld.timestamp.0 .0,
        other => other
            .get_int()
            .unwrap_or_else(|| panic!("expected Validity or Int tt, got {other:?}")),
    }
}

fn diag_code(err: &cozo::Error) -> Option<String> {
    err.code().map(|c| c.to_string())
}

fn now_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros() as i64
}

fn open_backend(engine: &str, dir: &std::path::Path, name: &str) -> DbInstance {
    match engine {
        "mem" => DbInstance::new("mem", "", "").unwrap(),
        "sqlite" => DbInstance::new("sqlite", dir.join(format!("{name}.sqlite")), "").unwrap(),
        "rocksdb" => DbInstance::new("rocksdb", dir.join(name), "").unwrap(),
        other => panic!("unsupported test engine: {other}"),
    }
}

fn assert_history_export_import_contract(engine: &str) {
    let dir = tempfile::tempdir().unwrap();
    let src = open_backend(engine, dir.path(), "src");

    run(
        &src,
        ":create doc {id: String, tt: TxTime => tag: String, body: String, emb: <F32; 2>}",
    );
    run(&src, "::index create doc:by_tag {tag, id}");
    run(
        &src,
        "::fts create doc:fts { extractor: body, extract_filter: true, tokenizer: Simple }",
    );
    run(
        &src,
        "::hnsw create doc:vec { dim: 2, m: 8, dtype: F32, fields: [emb], distance: Cosine, ef_construction: 16 }",
    );

    // T1: insert k1, k2, k3
    run(
        &src,
        "?[id, tag, body, emb] <- [
            ['k1', 'alpha', 'first contract net thirty', [1.0, 0.0]],
            ['k2', 'beta',  'second note beta',          [0.0, 1.0]],
            ['k3', 'gamma', 'third transient item',      [0.7, 0.7]]
         ] :put doc {id => tag, body, emb}",
    );
    let t1 = get_tt_us(&run(&src, "?[tt] := *doc{id: 'k1', tt}").rows[0][0]);

    // T2: update k1 (tag + body + emb) and retract k2
    run(
        &src,
        "?[id, tag, body, emb] <- [['k1', 'omega', 'updated contract net sixty', [0.0, 1.0]]]
         :put doc {id => tag, body, emb}",
    );
    run(&src, "?[id] <- [['k2']] :rm doc {id}");
    let t2 = get_tt_us(&run(&src, "?[tt] := *doc{id: 'k1', tt}").rows[0][0]);
    assert!(t2 > t1);

    // T3: re-assert k2 with new tag/body, evict k3 (populates mnestic_evict_audit)
    run(
        &src,
        "?[id, tag, body, emb] <- [['k2', 'reborn', 'reborn note delta', [1.0, 0.0]]]
         :put doc {id => tag, body, emb}",
    );
    run(&src, "::evict doc [['k3']]");
    let t3 = get_tt_us(&run(&src, "?[tt] := *doc{id: 'k2', tt}").rows[0][0]);
    assert!(t3 > t2);

    // Capture reference queries before export
    let ref_t1 = run(&src, &format!("?[id, tag, body] := *doc{{id, tag, body}} :as_of {t1} :order id")).into_json();
    let ref_t2 = run(&src, &format!("?[id, tag, body] := *doc{{id, tag, body}} :as_of {t2} :order id")).into_json();
    let ref_t3 = run(&src, &format!("?[id, tag, body] := *doc{{id, tag, body}} :as_of {t3} :order id")).into_json();
    let ref_cur = run(&src, "?[id, tag, body] := *doc{id, tag, body} :order id").into_json();
    let ref_hist_k1 = run(&src, "::history doc [['k1']]").into_json();
    let ref_hist_k2 = run(&src, "::history doc [['k2']]").into_json();
    let ref_audit = run(&src, "?[rel, key, tt, n] := *mnestic_evict_audit[rel, key, tt, n]").into_json();
    assert_eq!(ref_audit["rows"].as_array().unwrap().len(), 1);

    // 1. Export in one snapshot and round-trip through portable JSON (`into_json`/`from_json`)
    // AND Rust serde (`serde_json::to_string`/`from_str`).
    let bundle = src.export_txtime_state(["doc"].into_iter()).unwrap();
    assert!(bundle.tt_hwm >= t3, "exported tt_hwm must cover all commits");
    assert!(bundle.evict_audit.is_some());

    let portable_json = bundle.clone().into_json();
    let portable_str = serde_json::to_string(&portable_json).unwrap();
    let from_portable =
        TxTimeExportBundle::from_json(&serde_json::from_str(&portable_str).unwrap()).unwrap();

    for (label, import_bundle) in [("portable_json", from_portable), ("direct", bundle.clone())] {
        let dst = open_backend(engine, dir.path(), &format!("dst_{label}"));
        run(
            &dst,
            ":create doc {id: String, tt: TxTime => tag: String, body: String, emb: <F32; 2>}",
        );
        run(&dst, "::index create doc:by_tag {tag, id}");
        run(
            &dst,
            "::fts create doc:fts { extractor: body, extract_filter: true, tokenizer: Simple }",
        );
        run(
            &dst,
            "::hnsw create doc:vec { dim: 2, m: 8, dtype: F32, fields: [emb], distance: Cosine, ef_construction: 16 }",
        );

        let report: HistoryImportReport = dst.import_relations_with_history(import_bundle).unwrap();
        assert!(report.rows_imported >= 6, "{label}: expected raw history + audit rows");
        assert!(report.tt_hwm >= t3);
        assert!(
            report.indexes_needing_reindex.contains(&"doc:fts".to_string())
                && report.indexes_needing_reindex.contains(&"doc:vec".to_string()),
            "{label}: expected stranded search indexes, got {:?}",
            report.indexes_needing_reindex
        );

        // Stranded-index warning emitted on ::warnings
        let warnings = run(&dst, "::warnings").into_json();
        let warn_str = warnings.to_string();
        assert!(
            warn_str.contains("import.stranded_indexes"),
            "{label}: missing import.stranded_indexes warning: {warn_str}"
        );

        // Exact :as_of cuts & ::history byte-identity
        assert_eq!(
            run(&dst, &format!("?[id, tag, body] := *doc{{id, tag, body}} :as_of {t1} :order id")).into_json(),
            ref_t1,
            "{label}: :as_of t1 mismatch"
        );
        assert_eq!(
            run(&dst, &format!("?[id, tag, body] := *doc{{id, tag, body}} :as_of {t2} :order id")).into_json(),
            ref_t2,
            "{label}: :as_of t2 mismatch"
        );
        assert_eq!(
            run(&dst, &format!("?[id, tag, body] := *doc{{id, tag, body}} :as_of {t3} :order id")).into_json(),
            ref_t3,
            "{label}: :as_of t3 mismatch"
        );
        assert_eq!(
            run(&dst, "?[id, tag, body] := *doc{id, tag, body} :order id").into_json(),
            ref_cur,
            "{label}: current view mismatch"
        );
        assert_eq!(run(&dst, "::history doc [['k1']]").into_json(), ref_hist_k1);
        assert_eq!(run(&dst, "::history doc [['k2']]").into_json(), ref_hist_k2);
        assert_eq!(
            run(&dst, "?[rel, key, tt, n] := *mnestic_evict_audit[rel, key, tt, n]").into_json(),
            ref_audit
        );

        // Attached B-tree secondary index works immediately after history import
        let by_tag = run(&dst, "?[id] := *doc:by_tag{tag: 'omega', id}").into_json();
        assert_eq!(by_tag["rows"], json!([["k1"]]), "{label}: B-tree secondary index mismatch");
        let old_tag = run(&dst, "?[id] := *doc:by_tag{tag: 'alpha', id}").into_json();
        assert_eq!(old_tag["rows"], json!([]), "{label}: superseded tag leaked into B-tree index");

        // ::reindex restores FTS + HNSW cleanly
        run(&dst, "::reindex doc");
        let fts_hits = run(
            &dst,
            "?[id] := ~doc:fts{id | query: 'sixty', k: 5}",
        )
        .into_json();
        assert_eq!(fts_hits["rows"], json!([["k1"]]));
        let vec_hits = run(
            &dst,
            "?[id] := ~doc:vec{id | query: vec([1.0, 0.0]), k: 1, ef: 16}",
        )
        .into_json();
        assert_eq!(vec_hits["rows"], json!([["k2"]]));

        // Post-import write gets strictly greater tt
        run(
            &dst,
            "?[id, tag, body, emb] <- [['k4', 'fresh', 'new row', [1.0, 0.0]]]
             :put doc {id => tag, body, emb}",
        );
        let t4 = get_tt_us(&run(&dst, "?[tt] := *doc{id: 'k4', tt}").rows[0][0]);
        assert!(t4 > report.tt_hwm, "{label}: post-import tt {t4} <= hwm {}", report.tt_hwm);
    }

    // 2. ::history_gc floor round-trip: run GC on src, export, import into dst, verify floor enforced
    // First add another update to k1 so t1 is strictly superseded below t2
    let gc_res = run(&src, &format!("::history_gc doc {t2}"));
    let deleted = gc_res.rows[0][0].get_int().unwrap();
    assert!(deleted >= 1, "expected at least one superseded row collected");
    let gc_bundle = src.export_txtime_state(["doc"].into_iter()).unwrap();
    assert_eq!(gc_bundle.gc_floors.get("doc"), Some(&t2));

    let dst_gc = open_backend(engine, dir.path(), "dst_gc");
    run(
        &dst_gc,
        ":create doc {id: String, tt: TxTime => tag: String, body: String, emb: <F32; 2>}",
    );
    dst_gc
        .import_relations_with_history(
            TxTimeExportBundle::from_json(&gc_bundle.into_json()).unwrap(),
        )
        .unwrap();
    let below_err = run_err(
        &dst_gc,
        &format!("?[id] := *doc{{id}} :as_of {}", t2 - 1),
    );
    assert!(
        below_err.contains("eval::txtime_below_gc_floor"),
        "expected eval::txtime_below_gc_floor, got: {below_err}"
    );
    // At gc_floor itself still succeeds
    let at_floor = run(
        &dst_gc,
        &format!("?[id, tag, body] := *doc{{id, tag, body}} :as_of {t2} :order id"),
    )
    .into_json();
    assert_eq!(at_floor, ref_t2);

    // 3. Ordinary import_relations continues to reject user-supplied tt columns (tamper guard)
    let dst_ordinary = open_backend(engine, dir.path(), "dst_ordinary");
    run(
        &dst_ordinary,
        ":create doc {id: String, tt: TxTime => tag: String, body: String, emb: <F32; 2>}",
    );
    let ordinary_err = dst_ordinary
        .import_relations(bundle.relations.clone())
        .expect_err("ordinary import_relations must still reject tt column");
    assert!(
        format!("{ordinary_err:?}").contains("engine-assigned"),
        "unexpected ordinary import_relations error: {ordinary_err:?}"
    );
}

fn assert_fail_closed_guards(engine: &str) {
    let dir = tempfile::tempdir().unwrap();
    let db = open_backend(engine, dir.path(), "guards");
    run(&db, ":create r {k: String, tt: TxTime => v: Int}");

    // Non-empty target rejected with tx::history_import_non_empty
    run(&db, "?[k, v] <- [['a', 1]] :put r {k => v}");
    let bundle = db.export_txtime_state(["r"].into_iter()).unwrap();
    let err = db
        .import_relations_with_history(bundle.clone())
        .expect_err("non-empty target must be rejected");
    assert_eq!(diag_code(&err).as_deref(), Some("tx::history_import_non_empty"));

    // Exact schema mismatch (column name, type, nullability, key/non-key boundary, empty relation)
    // rejected with tx::import_schema_mismatch (Finding 2).
    for (label, bad_ddl) in [
        ("renamed_col", ":create r {k: String, tt: TxTime => wrong_col: Int}"),
        ("type_widening", ":create r {k: String, tt: TxTime => v: Float}"),
        ("nullability", ":create r {k: String, tt: TxTime => v: Int?}"),
        ("key_boundary", ":create r {k: String, v: Int, tt: TxTime}"),
        ("plain_axis", ":create r {k: String, tt: Int => v: Int}"),
    ] {
        let db_schema = open_backend(engine, dir.path(), &format!("schema_{label}"));
        run(&db_schema, bad_ddl);
        let err = db_schema
            .import_relations_with_history(bundle.clone())
            .expect_err(&format!("schema mismatch ({label}) must be rejected"));
        assert_eq!(
            diag_code(&err).as_deref(),
            Some("tx::import_schema_mismatch"),
            "{label}: unexpected diagnostic {err:?}"
        );
    }

    // Empty-relation schema mismatch also rejected with tx::import_schema_mismatch.
    let db_empty_src = open_backend(engine, dir.path(), "empty_src");
    run(&db_empty_src, ":create r {k: String, tt: TxTime => v: String}");
    let empty_bundle = db_empty_src.export_txtime_state(["r"].into_iter()).unwrap();
    let db_empty_dst = open_backend(engine, dir.path(), "empty_dst");
    run(&db_empty_dst, ":create r {k: String, tt: TxTime => v: Int}");
    let err = db_empty_dst
        .import_relations_with_history(empty_bundle)
        .expect_err("empty relation type mismatch must be rejected");
    assert_eq!(diag_code(&err).as_deref(), Some("tx::import_schema_mismatch"));

    // Negative tt rejected
    let db_neg = open_backend(engine, dir.path(), "neg");
    run(&db_neg, ":create r {k: String, tt: TxTime => v: Int}");
    let mut neg_bundle = db_neg.export_txtime_state(["r"].into_iter()).unwrap();
    neg_bundle.tt_hwm = 100;
    neg_bundle.relations.insert(
        "r".to_string(),
        NamedRows::new(
            vec!["k".into(), "tt".into(), "v".into()],
            vec![vec![
                DataValue::from("a"),
                DataValue::Validity(Validity::from((-5, true))),
                DataValue::from(1),
            ]],
        ),
    );
    let err = db_neg
        .import_relations_with_history(neg_bundle)
        .expect_err("negative tt must be rejected");
    assert_eq!(diag_code(&err).as_deref(), Some("tx::history_import_invalid_tt"));

    // Assert + retract same (key, tt) collision rejected with eval::txtime_assert_retract_conflict
    let db_col = open_backend(engine, dir.path(), "collision");
    run(&db_col, ":create r {k: String, tt: TxTime => v: Int}");
    let mut col_bundle = db_col.export_txtime_state(["r"].into_iter()).unwrap();
    col_bundle.tt_hwm = 1000;
    col_bundle.relations.insert(
        "r".to_string(),
        NamedRows::new(
            vec!["k".into(), "tt".into(), "v".into()],
            vec![
                vec![
                    DataValue::from("a"),
                    DataValue::Validity(Validity::from((500, true))),
                    DataValue::from(1),
                ],
                vec![
                    DataValue::from("a"),
                    DataValue::Validity(Validity::from((500, false))),
                    DataValue::from(1),
                ],
            ],
        ),
    );
    let err = db_col
        .import_relations_with_history(col_bundle)
        .expect_err("assert+retract collision must be rejected");
    assert_eq!(
        diag_code(&err).as_deref(),
        Some("eval::txtime_assert_retract_conflict")
    );

    // Corrupt gc_floor (two rows below floor for same logical key) rejected
    let db_gcf = open_backend(engine, dir.path(), "gcf");
    run(&db_gcf, ":create r {k: String, tt: TxTime => v: Int}");
    let mut gcf_bundle = db_gcf.export_txtime_state(["r"].into_iter()).unwrap();
    gcf_bundle.tt_hwm = 1000;
    gcf_bundle.gc_floors.insert("r".to_string(), 900);
    gcf_bundle.relations.insert(
        "r".to_string(),
        NamedRows::new(
            vec!["k".into(), "tt".into(), "v".into()],
            vec![
                vec![
                    DataValue::from("a"),
                    DataValue::Validity(Validity::from((100, true))),
                    DataValue::from(1),
                ],
                vec![
                    DataValue::from("a"),
                    DataValue::Validity(Validity::from((200, true))),
                    DataValue::from(2),
                ],
            ],
        ),
    );
    let err = db_gcf
        .import_relations_with_history(gcf_bundle)
        .expect_err("corrupt gc_floor must be rejected");
    assert_eq!(
        diag_code(&err).as_deref(),
        Some("tx::history_import_invalid_gc_floor")
    );

    // Stale destination tt_gc_floor (left behind after ::history_gc + ::evict)
    // is cleared when importing an un-GC'd bundle, so :as_of below the old floor
    // succeeds and subsequent re-export/re-import round-trips cleanly (Finding 3).
    let db_stale = open_backend(engine, dir.path(), "stale_floor");
    run(&db_stale, ":create r {k: String, tt: TxTime => v: Int}");
    run(&db_stale, "?[k, v] <- [['z', 1]] :put r {k => v}");
    run(&db_stale, "?[k, v] <- [['z', 2]] :put r {k => v}");
    let stale_cutoff = get_tt_us(&run(&db_stale, "?[tt] := *r{k: 'z', tt}").rows[0][0]);
    run(&db_stale, &format!("::history_gc r {stale_cutoff}"));
    run(&db_stale, "::evict r [['z']]");
    let mut un_gcd = db_stale.export_txtime_state(["r"].into_iter()).unwrap();
    un_gcd.gc_floors.clear();
    un_gcd.evict_audit = None;
    un_gcd.tt_hwm = 200;
    un_gcd.relations.insert(
        "r".to_string(),
        NamedRows::new(
            vec!["k".into(), "tt".into(), "v".into()],
            vec![
                vec![
                    DataValue::from("a"),
                    DataValue::Validity(Validity::from((100, true))),
                    DataValue::from(10),
                ],
                vec![
                    DataValue::from("a"),
                    DataValue::Validity(Validity::from((200, true))),
                    DataValue::from(20),
                ],
            ],
        ),
    );
    db_stale.import_relations_with_history(un_gcd).unwrap();
    assert_eq!(
        run(&db_stale, "?[v] := *r{k: 'a', v} :as_of 100").into_json()["rows"],
        json!([[10]]),
        "cleared stale gc_floor must allow :as_of below prior floor"
    );
    let re_exported = db_stale.export_txtime_state(["r", "mnestic_evict_audit"].into_iter()).unwrap();
    assert!(
        !re_exported.relations.contains_key("mnestic_evict_audit"),
        "mnestic_evict_audit must be routed exclusively to bundle.evict_audit"
    );
    assert!(re_exported.gc_floors.is_empty());
    assert!(re_exported.evict_audit.is_some());

    // ReadOnly mnestic_evict_audit blocks evict_audit import (Finding 5).
    let db_acl = open_backend(engine, dir.path(), "audit_acl");
    run(&db_acl, ":create r {k: String, tt: TxTime => v: Int}");
    run(&db_acl, "?[k, v] <- [['x', 1]] :put r {k => v}");
    run(&db_acl, "::evict r [['x']]");
    run(&db_acl, "::access_level read_only mnestic_evict_audit");
    let acl_err = db_acl
        .import_relations_with_history(re_exported)
        .expect_err("read_only mnestic_evict_audit must reject evict_audit import");
    assert!(
        format!("{acl_err:?}").contains("access") || format!("{acl_err:?}").contains("read_only"),
        "unexpected ACL error: {acl_err:?}"
    );
}

fn assert_clock_skew_monotonicity(engine: &str) {
    let dir = tempfile::tempdir().unwrap();
    let future_base = now_micros() + 24 * 3600 * 1_000_000; // +24h ahead of host wall clock
    let t_future_1 = future_base;
    let t_future_2 = future_base + 500;

    let t_post_import;
    {
        let db = open_backend(engine, dir.path(), "skew_db");
        run(&db, ":create skew {k: String, tt: TxTime => v: Int}");
        let mut bundle = db.export_txtime_state(["skew"].into_iter()).unwrap();
        bundle.tt_hwm = t_future_2;
        bundle.relations.insert(
            "skew".to_string(),
            NamedRows::new(
                vec!["k".into(), "tt".into(), "v".into()],
                vec![
                    vec![
                        DataValue::from("a"),
                        DataValue::Validity(Validity::from((t_future_1, true))),
                        DataValue::from(10),
                    ],
                    vec![
                        DataValue::from("a"),
                        DataValue::Validity(Validity::from((t_future_2, true))),
                        DataValue::from(20),
                    ],
                ],
            ),
        );
        let report = db.import_relations_with_history(bundle).unwrap();
        assert_eq!(report.tt_hwm, t_future_2);

        // Write while host wall clock is 24h BEHIND max_imported_tt
        run(&db, "?[k, v] <- [['a', 30]] :put skew {k => v}");
        t_post_import = get_tt_us(&run(&db, "?[tt] := *skew{k: 'a', tt}").rows[0][0]);
        assert!(
            t_post_import > t_future_2,
            "post-import tt {t_post_import} must strictly exceed future imported tt {t_future_2}"
        );
    }

    // For persistent backends, reopen from disk and confirm persisted tt_hwm survives restart
    if engine != "mem" {
        let reopened = open_backend(engine, dir.path(), "skew_db");
        run(&reopened, "?[k, v] <- [['a', 40]] :put skew {k => v}");
        let t_after_reopen = get_tt_us(&run(&reopened, "?[tt] := *skew{k: 'a', tt}").rows[0][0]);
        assert!(
            t_after_reopen > t_post_import,
            "post-reopen tt {t_after_reopen} must strictly exceed pre-close tt {t_post_import}"
        );
    }
}

fn assert_bitemporal_history_roundtrip(engine: &str) {
    let dir = tempfile::tempdir().unwrap();
    let src = open_backend(engine, dir.path(), "bi_src");
    run(&src, ":create facts {k: String, vt: Validity, tt: TxTime => v: String}");
    run(
        &src,
        "?[k, vt, v] <- [['e1', [1000000, true], 'v1']] :put facts {k, vt => v}",
    );
    let t1 = get_tt_us(&run(&src, "?[tt] := *facts{k: 'e1', tt}").rows[0][0]);
    run(
        &src,
        "?[k, vt, v] <- [['e1', [1000000, true], 'v2']] :put facts {k, vt => v}",
    );
    let t2 = get_tt_us(&run(&src, "?[tt] := *facts{k: 'e1', tt}").rows[0][0]);

    let bundle = src.export_txtime_state(["facts"].into_iter()).unwrap();
    let roundtripped = TxTimeExportBundle::from_json(&bundle.into_json()).unwrap();

    let dst = open_backend(engine, dir.path(), "bi_dst");
    run(&dst, ":create facts {k: String, vt: Validity, tt: TxTime => v: String}");
    dst.import_relations_with_history(roundtripped).unwrap();

    assert_eq!(
        run(&dst, &format!("?[v] := *facts{{k: 'e1', v @ (vt: 1000000, tt: {t1})}}")).into_json()["rows"],
        json!([["v1"]])
    );
    assert_eq!(
        run(&dst, &format!("?[v] := *facts{{k: 'e1', v @ (vt: 1000000, tt: {t2})}}")).into_json()["rows"],
        json!([["v2"]])
    );
}

#[cfg(feature = "storage-sqlite")]
fn assert_backup_and_selective_import_contract(engine: &str) {
    let dir = tempfile::tempdir().unwrap();
    let backup_path = dir.path().join("full.backup.sqlite");
    let future_base = now_micros() + 24 * 3600 * 1_000_000;

    // Build source store with future-clock history, retraction, gc_floor, evict_audit, B-tree, FTS, HNSW
    let (t1, t2, t_rm, t3, ref_t1, ref_t2, ref_rm, ref_cur, ref_hist, ref_audit);
    {
        let src = open_backend(engine, dir.path(), "bak_src");
        run(
            &src,
            ":create items {id: String, tt: TxTime => tag: String, text: String, emb: <F32; 2>}",
        );
        run(&src, "::index create items:by_tag {tag, id}");
        run(
            &src,
            "::fts create items:fts { extractor: text, extract_filter: true, tokenizer: Simple }",
        );
        run(
            &src,
            "::hnsw create items:vec { dim: 2, m: 8, dtype: F32, fields: [emb], distance: Cosine, ef_construction: 16 }",
        );

        // Seed future clock via history import so backup carries a +24h future HWM
        let mut seed = src.export_txtime_state(["items"].into_iter()).unwrap();
        seed.tt_hwm = future_base;
        seed.relations.insert(
            "items".to_string(),
            NamedRows::new(
                vec!["id".into(), "tt".into(), "tag".into(), "text".into(), "emb".into()],
                vec![
                    vec![
                        DataValue::from("i1"),
                        DataValue::Validity(Validity::from((future_base, true))),
                        DataValue::from("old"),
                        DataValue::from("legacy alpha"),
                        DataValue::List(vec![DataValue::from(1.0), DataValue::from(0.0)]),
                    ],
                    vec![
                        DataValue::from("i2"),
                        DataValue::Validity(Validity::from((future_base, true))),
                        DataValue::from("temp"),
                        DataValue::from("to be evicted"),
                        DataValue::List(vec![DataValue::from(0.0), DataValue::from(1.0)]),
                    ],
                ],
            ),
        );
        src.import_relations_with_history(seed).unwrap();
        run(&src, "::reindex items");
        t1 = future_base;

        // T2: update i1; T_rm: retract i1 (:rm); T3: re-assert i1, evict i2, GC t1
        run(
            &src,
            "?[id, tag, text, emb] <- [['i1', 'mid', 'middle beta', [0.7, 0.7]]]
             :put items {id => tag, text, emb}",
        );
        t2 = get_tt_us(&run(&src, "?[tt] := *items{id: 'i1', tt}").rows[0][0]);
        assert!(t2 > t1);

        run(&src, "?[id] <- [['i1']] :rm items {id}");
        let hist_after_rm = run(&src, "::history items [['i1']]");
        let tt_col = hist_after_rm
            .headers
            .iter()
            .position(|h| h == "tt")
            .unwrap();
        t_rm = hist_after_rm
            .rows
            .iter()
            .filter_map(|row| row.get(tt_col).map(get_tt_us))
            .max()
            .unwrap();
        assert!(t_rm > t2);

        run(
            &src,
            "?[id, tag, text, emb] <- [['i1', 'cur', 'current gamma', [0.0, 1.0]]]
             :put items {id => tag, text, emb}",
        );
        run(&src, "::evict items [['i2']]");
        t3 = get_tt_us(&run(&src, "?[tt] := *items{id: 'i1', tt}").rows[0][0]);
        assert!(t3 > t_rm);

        // Collect superseded version at t1 -> sets gc_floor = t2
        let gc = run(&src, &format!("::history_gc items {t2}"));
        assert!(gc.rows[0][0].get_int().unwrap() >= 1);

        ref_t1 = t1;
        ref_t2 = run(&src, &format!("?[id, tag] := *items{{id, tag}} :as_of {t2}")).into_json();
        ref_rm = run(&src, &format!("?[id, tag] := *items{{id, tag}} :as_of {t_rm}")).into_json();
        assert_eq!(ref_rm["rows"], json!([]), "retracted i1 must be absent at t_rm");
        ref_cur = run(&src, "?[id, tag] := *items{id, tag}").into_json();
        ref_hist = run(&src, "::history items [['i1']]").into_json();
        ref_audit = run(&src, "?[r, k, tt, n] := *mnestic_evict_audit[r, k, tt, n]").into_json();

        src.backup_db(&backup_path).unwrap();
    }

    // A. Physical restore_backup into a brand-new destination store
    let t4;
    {
        let restored = open_backend(engine, dir.path(), "bak_restored");
        restored.restore_backup(&backup_path).unwrap();

        // History, :as_of (including retraction boundary t_rm), gc_floor, evict_audit all preserved verbatim
        assert_eq!(
            run(&restored, &format!("?[id, tag] := *items{{id, tag}} :as_of {t2}")).into_json(),
            ref_t2
        );
        assert_eq!(
            run(&restored, &format!("?[id, tag] := *items{{id, tag}} :as_of {t_rm}")).into_json(),
            ref_rm
        );
        assert_eq!(run(&restored, "?[id, tag] := *items{id, tag}").into_json(), ref_cur);
        assert_eq!(run(&restored, "::history items [['i1']]").into_json(), ref_hist);
        assert_eq!(
            run(&restored, "?[r, k, tt, n] := *mnestic_evict_audit[r, k, tt, n]").into_json(),
            ref_audit
        );
        let below = run_err(&restored, &format!("?[id] := *items{{id}} :as_of {}", ref_t1));
        assert!(
            below.contains("eval::txtime_below_gc_floor"),
            "gc_floor not preserved by restore_backup: {below}"
        );

        // B-tree, FTS, and HNSW indexes work after restore_backup
        assert_eq!(
            run(&restored, "?[id] := *items:by_tag{tag: 'cur', id}").into_json()["rows"],
            json!([["i1"]])
        );
        assert_eq!(
            run(&restored, "?[id] := ~items:fts{id | query: 'gamma', k: 5}").into_json()["rows"],
            json!([["i1"]])
        );
        assert_eq!(
            run(&restored, "?[id] := ~items:vec{id | query: vec([0.0, 1.0]), k: 1, ef: 16}")
                .into_json()["rows"],
            json!([["i1"]])
        );

        // Monotonicity across backward wall clock jump (+24h HWM vs current wall clock)
        run(
            &restored,
            "?[id, tag, text, emb] <- [['i3', 'new', 'post restore', [1.0, 0.0]]]
             :put items {id => tag, text, emb}",
        );
        t4 = get_tt_us(&run(&restored, "?[tt] := *items{id: 'i3', tt}").rows[0][0]);
        assert!(t4 > t3, "post-restore tt {t4} must exceed backup tt {t3}");
    }
    // Reopen restored persistent store from disk and verify persisted tt_hwm survives restart.
    {
        let reopened = open_backend(engine, dir.path(), "bak_restored");
        run(
            &reopened,
            "?[id, tag, text, emb] <- [['i5', 'reopen', 'after restart', [1.0, 0.0]]]
             :put items {id => tag, text, emb}",
        );
        let t5 = get_tt_us(&run(&reopened, "?[tt] := *items{id: 'i5', tt}").rows[0][0]);
        assert!(t5 > t4, "post-reopen tt {t5} must exceed pre-close tt {t4}");
    }

    // B. Selective import_from_backup into an empty matching TxTime relation (no B-tree indices)
    {
        let selective = open_backend(engine, dir.path(), "bak_selective");
        run(
            &selective,
            ":create items {id: String, tt: TxTime => tag: String, text: String, emb: <F32; 2>}",
        );
        run(
            &selective,
            "::fts create items:fts { extractor: text, extract_filter: true, tokenizer: Simple }",
        );

        selective
            .import_from_backup(&backup_path, &["items".to_string()])
            .unwrap();

        // History, :as_of (including retraction boundary t_rm), gc_floor preserved; stranded-index warning emitted
        assert_eq!(
            run(&selective, &format!("?[id, tag] := *items{{id, tag}} :as_of {t2}")).into_json(),
            ref_t2
        );
        assert_eq!(
            run(&selective, &format!("?[id, tag] := *items{{id, tag}} :as_of {t_rm}")).into_json(),
            ref_rm
        );
        assert_eq!(run(&selective, "::history items [['i1']]").into_json(), ref_hist);
        let below = run_err(&selective, &format!("?[id] := *items{{id}} :as_of {}", ref_t1));
        assert!(
            below.contains("eval::txtime_below_gc_floor"),
            "gc_floor not preserved by import_from_backup: {below}"
        );
        let warn_str = run(&selective, "::warnings").into_json().to_string();
        assert!(
            warn_str.contains("import.stranded_indexes"),
            "missing stranded index warning: {warn_str}"
        );

        // Post-import write stays monotonic above backup's +24h future HWM
        run(
            &selective,
            "?[id, tag, text, emb] <- [['i4', 'sel', 'after selective', [1.0, 0.0]]]
             :put items {id => tag, text, emb}",
        );
        let t_sel = get_tt_us(&run(&selective, "?[tt] := *items{id: 'i4', tt}").rows[0][0]);
        assert!(t_sel > t3, "selective import_from_backup must advance tt_hwm past {t3}");

        // Second import_from_backup into now non-empty TxTime relation is rejected
        let err = selective
            .import_from_backup(&backup_path, &["items".to_string()])
            .expect_err("non-empty TxTime import_from_backup must fail");
        assert_eq!(diag_code(&err).as_deref(), Some("tx::history_import_non_empty"));
    }
}

#[test]
fn txtime_history_export_import_mem() {
    assert_history_export_import_contract("mem");
    assert_fail_closed_guards("mem");
    assert_clock_skew_monotonicity("mem");
    assert_bitemporal_history_roundtrip("mem");
}

#[cfg(feature = "storage-sqlite")]
#[test]
fn txtime_history_export_import_sqlite() {
    assert_history_export_import_contract("sqlite");
    assert_fail_closed_guards("sqlite");
    assert_clock_skew_monotonicity("sqlite");
    assert_bitemporal_history_roundtrip("sqlite");
    assert_backup_and_selective_import_contract("sqlite");
}

#[cfg(all(feature = "storage-rocksdb", feature = "storage-sqlite"))]
#[test]
fn txtime_history_export_import_rocksdb() {
    assert_history_export_import_contract("rocksdb");
    assert_fail_closed_guards("rocksdb");
    assert_clock_skew_monotonicity("rocksdb");
    assert_bitemporal_history_roundtrip("rocksdb");
    assert_backup_and_selective_import_contract("rocksdb");
}
