/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file, You can obtain one at
 * https://mozilla.org/MPL/2.0/.
 */

//! EA4 Batch 4.2 acceptance tests (`docs/plans/ea4-txtime-engine-batches.md`):
//! - Commit-section triggers (`::set_triggers`) on `tt-only` relations
//! - Always-fresh current-belief cached graph projections (`::graph create`) over
//!   `tt-only` relations, plus ephemeral `:as_of` historical graph queries.

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

#[test]
fn txtime_triggers_maintain_derived_tables_across_put_update_rm_evict_and_gc() {
    engines(|db| {
        run(
            db,
            ":create edge_tt {uid: String, tt: TxTime => from_uid: String, to_uid: String, weight: Float}",
        );
        run(
            db,
            ":create traversal_cost {from_uid: String, to_uid: String, edge_uid: String => cost: Float}",
        );
        run(
            db,
            ":create traversal_cost_rev {to_uid: String, from_uid: String, edge_uid: String => cost: Float}",
        );
        run(
            db,
            ":create trigger_log {kind: String, uid: String, tt_int: Int => prev_to: String, new_to: String}",
        );

        // MindGraph-shaped derived pair maintenance: remove stale (from, to) on update/retarget,
        // insert new (from, to, cost), and remove on retraction/eviction.
        run(
            db,
            r#"::set_triggers edge_tt
on put {
    ?[from_uid, to_uid, edge_uid] := _old[edge_uid, _, from_uid, to_uid, _]
    :rm traversal_cost {from_uid, to_uid, edge_uid}
}
on put {
    ?[to_uid, from_uid, edge_uid] := _old[edge_uid, _, from_uid, to_uid, _]
    :rm traversal_cost_rev {to_uid, from_uid, edge_uid}
}
on put {
    ?[from_uid, to_uid, edge_uid, cost] := _new[edge_uid, _, from_uid, to_uid, weight], cost = 1.0 / weight
    :put traversal_cost {from_uid, to_uid, edge_uid => cost}
}
on put {
    ?[to_uid, from_uid, edge_uid, cost] := _new[edge_uid, _, from_uid, to_uid, weight], cost = 1.0 / weight
    :put traversal_cost_rev {to_uid, from_uid, edge_uid => cost}
}
on put {
    ?[kind, uid, tt_int, prev_to, new_to] :=
        _new[uid, tt, _, new_to, _],
        tt_int = to_int(tt),
        kind = 'put',
        prev_to = ''
    :put trigger_log {kind, uid, tt_int => prev_to, new_to}
}
on rm {
    ?[from_uid, to_uid, edge_uid] := _old[edge_uid, _, from_uid, to_uid, _]
    :rm traversal_cost {from_uid, to_uid, edge_uid}
}
on rm {
    ?[to_uid, from_uid, edge_uid] := _old[edge_uid, _, from_uid, to_uid, _]
    :rm traversal_cost_rev {to_uid, from_uid, edge_uid}
}
on rm {
    ?[kind, uid, tt_int, prev_to, new_to] :=
        _old[uid, _, _, prev_to, _],
        _new[uid, tt],
        tt_int = to_int(tt),
        kind = 'rm',
        new_to = ''
    :put trigger_log {kind, uid, tt_int => prev_to, new_to}
}"#,
        );

        // 1. Insert edge e1 (a -> b, weight 2.0 => cost 0.5).
        run(
            db,
            "?[uid, from_uid, to_uid, weight] <- [['e1', 'a', 'b', 2.0], ['e2', 'b', 'c', 4.0]] :put edge_tt {uid => from_uid, to_uid, weight}",
        );
        let t1 = max_tt(db, "edge_tt");
        assert_eq!(
            run_ro(
                db,
                "?[from_uid, to_uid, edge_uid, cost] := *traversal_cost{from_uid, to_uid, edge_uid, cost} :order edge_uid"
            )
            .rows,
            vec![
                vec![
                    DataValue::from("a"),
                    DataValue::from("b"),
                    DataValue::from("e1"),
                    DataValue::from(0.5),
                ],
                vec![
                    DataValue::from("b"),
                    DataValue::from("c"),
                    DataValue::from("e2"),
                    DataValue::from(0.25),
                ],
            ]
        );
        // Trigger observed engine-assigned `tt` on `_new`.
        assert_eq!(
            run_ro(
                db,
                &format!("?[uid, new_to] := *trigger_log{{kind: 'put', uid, tt_int: {t1}, new_to}} :order uid")
            )
            .rows,
            vec![
                vec![DataValue::from("e1"), DataValue::from("b")],
                vec![DataValue::from("e2"), DataValue::from("c")],
            ]
        );

        // 2. Retarget e1 from (a -> b) to (a -> d) with weight 0.5 => cost 2.0; old (a -> b) removed.
        run(
            db,
            "?[uid, from_uid, to_uid, weight] <- [['e1', 'a', 'd', 0.5]] :put edge_tt {uid => from_uid, to_uid, weight}",
        );
        let t2 = max_tt(db, "edge_tt");
        assert_eq!(
            run_ro(
                db,
                "?[from_uid, to_uid, edge_uid, cost] := *traversal_cost{from_uid, to_uid, edge_uid, cost} :order edge_uid"
            )
            .rows,
            vec![
                vec![
                    DataValue::from("a"),
                    DataValue::from("d"),
                    DataValue::from("e1"),
                    DataValue::from(2.0),
                ],
                vec![
                    DataValue::from("b"),
                    DataValue::from("c"),
                    DataValue::from("e2"),
                    DataValue::from(0.25),
                ],
            ]
        );
        assert_eq!(
            run_ro(
                db,
                "?[to_uid, from_uid, edge_uid] := *traversal_cost_rev{to_uid, from_uid, edge_uid} :order edge_uid"
            )
            .rows,
            vec![
                vec![
                    DataValue::from("d"),
                    DataValue::from("a"),
                    DataValue::from("e1"),
                ],
                vec![
                    DataValue::from("c"),
                    DataValue::from("b"),
                    DataValue::from("e2"),
                ],
            ]
        );

        // 3. Retract e1 via `:rm`: fires `on rm`, removes from derived tables.
        run(db, "?[uid] <- [['e1']] :rm edge_tt {uid}");
        assert_eq!(
            run_ro(
                db,
                "?[edge_uid] := *traversal_cost{edge_uid} :order edge_uid"
            )
            .rows,
            vec![vec![DataValue::from("e2")]]
        );
        let rm_logs_after_rm = run_ro(
            db,
            "?[uid, prev_to] := *trigger_log{kind: 'rm', uid, prev_to} :order uid",
        )
        .rows;
        assert_eq!(
            rm_logs_after_rm,
            vec![vec![DataValue::from("e1"), DataValue::from("d")]]
        );

        // 4. `::history_gc` fires NO triggers.
        let log_count_before_gc = run_ro(db, "?[count(uid)] := *trigger_log{uid}").rows[0][0]
            .get_int()
            .unwrap();
        run(db, &format!("::history_gc edge_tt {t2}"));
        let log_count_after_gc = run_ro(db, "?[count(uid)] := *trigger_log{uid}").rows[0][0]
            .get_int()
            .unwrap();
        assert_eq!(log_count_before_gc, log_count_after_gc);

        // 5. `::evict` on already-retracted e1 fires NO `on rm`; `::evict` on live e2 fires `on rm`.
        run(db, "::evict edge_tt [['e1']]");
        assert_eq!(
            run_ro(db, "?[count(uid)] := *trigger_log{uid}").rows[0][0]
                .get_int()
                .unwrap(),
            log_count_after_gc
        );
        run(db, "::evict edge_tt [['e2']]");
        assert!(run_ro(db, "?[edge_uid] := *traversal_cost{edge_uid}")
            .rows
            .is_empty());
        assert!(run_ro(db, "?[edge_uid] := *traversal_cost_rev{edge_uid}")
            .rows
            .is_empty());
        assert_eq!(
            run_ro(
                db,
                "?[uid, prev_to] := *trigger_log{kind: 'rm', uid, prev_to} :order uid"
            )
            .rows,
            vec![
                vec![DataValue::from("e1"), DataValue::from("d")],
                vec![DataValue::from("e2"), DataValue::from("c")],
            ]
        );

        // 6. `import_relations` does NOT fire triggers (D4.2.2.5).
        let imported = NamedRows::new(
            vec![
                "uid".into(),
                "from_uid".into(),
                "to_uid".into(),
                "weight".into(),
            ],
            vec![vec![
                DataValue::from("e9"),
                DataValue::from("x"),
                DataValue::from("y"),
                DataValue::from(1.0),
            ]],
        );
        db.import_relations(BTreeMap::from([("edge_tt".to_string(), imported)]))
            .unwrap();
        assert_eq!(
            run_ro(db, "?[uid] := *edge_tt{uid}").rows,
            vec![vec![DataValue::from("e9")]]
        );
        assert!(run_ro(db, "?[edge_uid] := *traversal_cost{edge_uid}")
            .rows
            .is_empty());
    });
}

#[test]
fn txtime_trigger_error_and_nested_tt_write_rollback_atomically() {
    engines(|db| {
        run(db, ":create src_tt {k: String, tt: TxTime => v: Int}");
        run(db, ":create other_tt {k: String, tt: TxTime => v: Int}");
        run(db, ":create plain_log {k: String => v: Int}");

        // Trigger that fails on v < 0 must roll back both base TxTime write and plain_log write.
        run(
            db,
            r#"::set_triggers src_tt
on put {
    ?[k, v] := _new[k, _, v]
    :put plain_log {k => v}
}
on put {
    ?[k] := _new[k, _, v], v < 0
    :assert none
}"#,
        );

        run(db, "?[k, v] <- [['ok', 1]] :put src_tt {k => v}");
        assert!(db
            .run_script(
                "?[k, v] <- [['bad', -5]] :put src_tt {k => v}",
                BTreeMap::new(),
                ScriptMutability::Mutable,
            )
            .is_err());
        assert_eq!(
            run_ro(db, "?[k, v] := *src_tt{k, v}").rows,
            vec![vec![DataValue::from("ok"), DataValue::from(1)]]
        );
        assert_eq!(
            run_ro(db, "?[k, v] := *plain_log{k, v}").rows,
            vec![vec![DataValue::from("ok"), DataValue::from(1)]]
        );

        // Static ::set_triggers check rejects writing to an existing TxTime relation.
        let static_err = db
            .run_script(
                r#"::set_triggers src_tt
on put {
    ?[k, v] := _new[k, _, v]
    :put other_tt {k => v}
}"#,
                BTreeMap::new(),
                ScriptMutability::Mutable,
            )
            .unwrap_err();
        assert!(
            format!("{static_err:?}").contains("eval::txtime_trigger_nested_tt_write"),
            "expected static txtime_trigger_nested_tt_write, got: {static_err:?}"
        );

        // Dynamic runtime check: install trigger targeting `late_tt` while it is plain,
        // replace `late_tt` with a TxTime relation, then verify `:put src_tt` fails with
        // eval::txtime_trigger_nested_tt_write and rolls back atomically (even on 0-row filter).
        run(db, ":create late_tt {k: String => v: Int}");
        run(
            db,
            r#"::set_triggers src_tt
on put {
    ?[k, v] := _new[k, _, v]
    :put late_tt {k => v}
}"#,
        );
        run(db, "::remove late_tt");
        run(db, ":create late_tt {k: String, tt: TxTime => v: Int}");

        let err = db
            .run_script(
                "?[k, v] <- [['nested', 9]] :put src_tt {k => v}",
                BTreeMap::new(),
                ScriptMutability::Mutable,
            )
            .unwrap_err();
        assert!(
            format!("{err:?}").contains("eval::txtime_trigger_nested_tt_write"),
            "expected runtime txtime_trigger_nested_tt_write, got: {err:?}"
        );
        assert_eq!(
            run_ro(db, "?[k] := *src_tt{k}").rows,
            vec![vec![DataValue::from("ok")]]
        );
        assert!(run_ro(db, "?[k] := *late_tt{k}").rows.is_empty());

        // Imperative ::set_triggers on a TxTime relation must be rejected as standalone DDL.
        let imp_err = db
            .run_script(
                "{ ::set_triggers src_tt }",
                BTreeMap::new(),
                ScriptMutability::Mutable,
            )
            .unwrap_err();
        assert!(
            format!("{imp_err:?}").contains("eval::txtime_ddl_standalone"),
            "expected standalone ddl error, got: {imp_err:?}"
        );
    });
}

#[cfg(feature = "graph-algo")]
#[test]
fn graph_projection_over_tt_only_tracks_current_belief_and_serves_ephemeral_as_of() {
    engines(|db| {
        run(
            db,
            ":create e_tt {from: String, to: String, tt: TxTime => weight: Float}",
        );
        run(db, "::graph create g {edges: e_tt}");

        // T1: a -> b (weight 1.0), b -> c (weight 1.0) => shortest path a -> c has cost 2.0.
        run(
            db,
            "?[from, to, weight] <- [['a', 'b', 1.0], ['b', 'c', 1.0]] :put e_tt {from, to => weight}",
        );
        let t1 = max_tt(db, "e_tt");
        let sp_t1 = run_ro(
            db,
            "s[x] <- [['a']] g_end[x] <- [['c']] ?[s, d, cost, path] <~ ShortestPathDijkstra(s[], g_end[], graph: g)",
        );
        assert_eq!(
            sp_t1.rows,
            vec![vec![
                DataValue::from("a"),
                DataValue::from("c"),
                DataValue::from(2.0),
                DataValue::List(vec![
                    DataValue::from("a"),
                    DataValue::from("b"),
                    DataValue::from("c"),
                ]),
            ]]
        );

        // T2: update a -> b weight, add direct edge a -> c (weight 0.5), and retract b -> c; cache must invalidate immediately.
        run(
            db,
            "?[from, to, weight] <- [['a', 'b', 5.0], ['a', 'c', 0.5]] :put e_tt {from, to => weight}",
        );
        run(db, "?[from, to] <- [['b', 'c']] :rm e_tt {from, to}");
        let t2 = max_tt(db, "e_tt");

        let sp_cur = run_ro(
            db,
            "s[x] <- [['a']] g_end[x] <- [['c']] ?[s, d, cost, path] <~ ShortestPathDijkstra(s[], g_end[], graph: g)",
        );
        assert_eq!(
            sp_cur.rows,
            vec![vec![
                DataValue::from("a"),
                DataValue::from("c"),
                DataValue::from(0.5),
                DataValue::List(vec![DataValue::from("a"), DataValue::from("c")]),
            ]]
        );

        // Historical `:as_of T1` must return the T1 graph (cost 2.0 via b) WITHOUT polluting current cache!
        let sp_asof_t1 = run_ro(
            db,
            &format!(
                "s[x] <- [['a']] g_end[x] <- [['c']] ?[s, d, cost, path] <~ ShortestPathDijkstra(s[], g_end[], graph: g) :as_of {t1}"
            ),
        );
        assert_eq!(
            sp_asof_t1.rows,
            vec![vec![
                DataValue::from("a"),
                DataValue::from("c"),
                DataValue::from(2.0),
                DataValue::List(vec![
                    DataValue::from("a"),
                    DataValue::from("b"),
                    DataValue::from("c"),
                ]),
            ]]
        );

        // Current query immediately after `:as_of T1` still returns current belief (0.5).
        let sp_cur_after = run_ro(
            db,
            "s[x] <- [['a']] g_end[x] <- [['c']] ?[s, d, cost, path] <~ ShortestPathDijkstra(s[], g_end[], graph: g)",
        );
        assert_eq!(
            sp_cur_after.rows,
            vec![vec![
                DataValue::from("a"),
                DataValue::from("c"),
                DataValue::from(0.5),
                DataValue::List(vec![DataValue::from("a"), DataValue::from("c")]),
            ]]
        );

        // GC past T1: `:as_of T1` over projection fails with eval::txtime_below_gc_floor.
        run(db, &format!("::history_gc e_tt {t2}"));
        let gc_err = db
            .run_script(
                &format!(
                    "s[x] <- [['a']] g_end[x] <- [['c']] ?[s, d, cost, path] <~ ShortestPathDijkstra(s[], g_end[], graph: g) :as_of {t1}"
                ),
                BTreeMap::new(),
                ScriptMutability::Immutable,
            )
            .unwrap_err();
        assert!(
            format!("{gc_err:?}").contains("eval::txtime_below_gc_floor"),
            "expected txtime_below_gc_floor, got: {gc_err:?}"
        );

        // Trigger-maintained plain table projection freshness on commit AND abort.
        run(db, ":create e_src {a: String, b: String, tt: TxTime => w: Float}");
        run(db, ":create derived_cost {a: String, b: String => cost: Float}");
        run(
            db,
            "::set_triggers e_src
                on put { ?[a, b, cost] := _new[a, b, _, w], cost = 1.0 / w :put derived_cost {a, b => cost} }
                on rm  { ?[a, b] := _old[a, b, _, _] :rm derived_cost {a, b} }",
        );
        run(db, "::graph create g_derived {edges: derived_cost}");
        run(db, "?[a, b, w] <- [['a', 'b', 0.5], ['b', 'c', 0.5]] :put e_src {a, b => w}");
        let d1 = run_ro(
            db,
            "s[x] <- [['a']] d[x] <- [['c']] ?[s, d, cost, path] <~ ShortestPathDijkstra(s[], d[], graph: g_derived)",
        );
        assert_eq!(d1.rows[0][2], DataValue::from(4.0));

        // Update e_src -> trigger updates derived_cost -> projection invalidates immediately!
        run(db, "?[a, b, w] <- [['a', 'c', 2.0]] :put e_src {a, b => w}");
        let d2 = run_ro(
            db,
            "s[x] <- [['a']] d[x] <- [['c']] ?[s, d, cost, path] <~ ShortestPathDijkstra(s[], d[], graph: g_derived)",
        );
        assert_eq!(d2.rows[0][2], DataValue::from(0.5));

        // Static ::set_triggers rejection of nested TxTime write (self, other tt, :create tt)
        run(db, ":create other_tt {k: String, tt: TxTime => v: Int}");
        for bad_trigger in [
            "::set_triggers e_src on put { ?[a, b, w] := _new[a, b, _, w], w > 999.0 :put e_src {a, b => w} }",
            "::set_triggers e_src on put { ?[k, v] := _new[k, _, _, w], w > 999.0, v = 1 :put other_tt {k => v} }",
            "::set_triggers e_src on put { ?[k, v] := _new[k, _, _, w], w > 999.0, v = 1 :reconcile other_tt {k => v} }",
            "::set_triggers e_src on put { ?[k, v] := _new[k, _, _, w], w > 999.0, v = 1 :create brand_new_tt {k: String, tt: TxTime => v: Int} }",
        ] {
            let err = db
                .run_script(bad_trigger, BTreeMap::new(), ScriptMutability::Mutable)
                .expect_err(&format!("expected nested tt write rejection for: {bad_trigger}"));
            assert!(
                format!("{err:?}").contains("eval::txtime_trigger_nested_tt_write"),
                "expected eval::txtime_trigger_nested_tt_write for {bad_trigger}, got: {err:?}"
            );
        }
    });
}

#[cfg(feature = "storage-rocksdb")]
#[test]
fn txtime_triggers_concurrent_rocksdb_writers() {
    let dir = tempfile::tempdir().unwrap();
    let db = std::sync::Arc::new(
        DbInstance::new("rocksdb", dir.path().join("rocks_triggers"), "").unwrap(),
    );
    run(&db, ":create edge_c {id: String, tt: TxTime => w: Int}");
    run(&db, ":create cost_c {id: String => c: Int}");
    run(
        &db,
        "::set_triggers edge_c
            on put { ?[id, c] := _new[id, _, w], c = w * 2 :put cost_c {id => c} }
            on rm  { ?[id] := _old[id, _, _] :rm cost_c {id} }",
    );

    let mut handles = Vec::new();
    for worker in 0..4usize {
        let db = db.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..25usize {
                let id = format!("w{worker}_k{i}");
                run(
                    &db,
                    &format!("?[id, w] <- [['{id}', {}]] :put edge_c {{id => w}}", i + 1),
                );
                run(
                    &db,
                    &format!("?[id, w] <- [['{id}', {}]] :put edge_c {{id => w}}", i + 10),
                );
                if i % 5 == 0 {
                    run(&db, &format!("?[id] <- [['{id}']] :rm edge_c {{id}}"));
                }
                // Also contend across workers on one shared hot key.
                run(
                    &db,
                    &format!(
                        "?[id, w] <- [['shared_hot', {}]] :put edge_c {{id => w}}",
                        worker * 100 + i
                    ),
                );
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    // Derived table must exactly match fresh recomputation from current beliefs of edge_c.
    let expected = run_ro(&db, "?[id, c] := *edge_c{id, w}, c = w * 2 :order id");
    let actual = run_ro(&db, "?[id, c] := *cost_c{id, c} :order id");
    assert_eq!(actual.rows.len(), 4 * 20 + 1, "expected 80 disjoint + 1 shared key");
    assert_eq!(actual.rows, expected.rows);
}
