/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file, You can obtain one at
 * https://mozilla.org/MPL/2.0/.
 */

//! Differential correctness and physical-plan contracts for local keyed access.
#![cfg(feature = "test-hooks")]

use cozo::{DbInstance, NamedRows, ScriptMutability};
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn run(db: &DbInstance, script: &str) -> NamedRows {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap_or_else(|e| panic!("{script}\n{e:?}"))
}

fn rows(db: &DbInstance, script: &str) -> Value {
    run(db, script).into_json()["rows"].clone()
}

fn engines(mut test: impl FnMut(&DbInstance)) {
    let dir = tempfile::tempdir().unwrap();
    let mut names = vec!["mem"];
    #[cfg(feature = "storage-sqlite")]
    names.push("sqlite");
    #[cfg(feature = "storage-rocksdb")]
    names.push("rocksdb");
    for engine in names {
        let path = dir.path().join(engine);
        let db = DbInstance::new(engine, path.to_str().unwrap(), "{}").unwrap();
        test(&db);
    }
}

fn equivalent(db: &DbInstance, query: &str, expected: Value) {
    for enabled in [false, true] {
        db.set_keyed_access_for_tests(enabled);
        assert_eq!(rows(db, query), expected, "enabled={enabled}: {query}");
    }
}

fn plan(db: &DbInstance, query: &str, enabled: bool) -> Vec<BTreeMap<String, Value>> {
    db.set_keyed_access_for_tests(enabled);
    let result = run(db, &format!("::explain {{ {query} }}")).into_json();
    let headers = result["headers"].as_array().unwrap();
    result["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            headers
                .iter()
                .zip(row.as_array().unwrap())
                .map(|(h, v)| (h.as_str().unwrap().to_owned(), v.clone()))
                .collect()
        })
        .collect()
}

fn ops(plan: &[BTreeMap<String, Value>]) -> Vec<&str> {
    plan.iter().map(|r| r["op"].as_str().unwrap()).collect()
}

fn refs(plan: &[BTreeMap<String, Value>]) -> Vec<&Value> {
    plan.iter()
        .filter(|r| r["op"].as_str().unwrap().starts_with("load_stored"))
        .map(|r| &r["ref"])
        .collect()
}

#[test]
fn scalar_producers_and_generator_multiplicity_survive() {
    engines(|db| {
        run(db, ":create m {k:Int}");
        run(db, "?[k] <- [[5]] :put m{k}");
        equivalent(db, "?[k] := k=5.0, *m{k}, k=5", json!([]));
        run(db, ":create s {k:String}");
        run(db, "?[k] <- [['a']] :put s{k}");
        equivalent(
            db,
            "?[count(k)] := k in ['a','a'], *s{k}, k='a'",
            json!([[2]]),
        );
        equivalent(db, "?[count(k)] := z in [1,1], *s{k}, k='a'", json!([[2]]));
        equivalent(
            db,
            "?[collect(k)] := k in ['a','a'], *s{k}, k='a'",
            json!([[['a', 'a']]]),
        );
        equivalent(db, "?[k] := k in [], *s{k}, k='a'", json!([]));
    });
}

#[test]
fn constants_and_rule_aliases_enable_points_without_changing_results() {
    engines(|db| {
        run(db, ":create node {uid:String => name:String}");
        run(
            db,
            "?[uid,name] <- [['a','alpha'],['b','beta']] :put node{uid=>name}",
        );
        for suffix in ["", ":limit 1", ":reorder written"] {
            let query = format!("?[uid,name] := *node{{uid,name}}, uid='a' {suffix}");
            equivalent(db, &query, json!([["a", "alpha"]]));
            assert!(ops(&plan(db, &query, true)).contains(&"stored_point_join"));
        }
        let query = "f[x] <- [['a']] ?[u,n] := f[x], *node{uid:u,name:n}, u=x :reorder written";
        equivalent(db, query, json!([["a", "alpha"]]));
        assert!(ops(&plan(db, query, true)).contains(&"stored_point_join"));
    });
}

#[test]
fn full_key_with_nonadjacent_residual_uses_point_and_checks_value() {
    engines(|db| {
        run(db, ":create n {id:String => a:String, b:String}");
        run(
            db,
            "?[id,a,b] <- [['x','gap','yes'],['y','gap','no']] :put n{id=>a,b}",
        );
        for (value, want) in [("yes", json!([["x"]])), ("no", json!([]))] {
            let q = format!("?[id] := id='x', b='{value}', *n{{id,a,b}} :reorder written");
            equivalent(db, &q, want);
            assert!(ops(&plan(db, &q, false)).contains(&"stored_mat_join"));
            let optimized = plan(db, &q, true);
            let point = optimized
                .iter()
                .find(|r| r["op"] == "stored_point_join")
                .unwrap();
            assert_eq!(point["joins_on"].as_object().unwrap().len(), 1);
            assert!(point.values().any(|v| v.as_array().is_some_and(|a| a
                .iter()
                .any(|v| v.as_str().is_some_and(|s| s.contains("=="))))));
        }
    });
}

#[test]
fn partial_composite_key_never_becomes_point() {
    engines(|db| {
        run(db, ":create c {a:String,b:String => c:String,d:String}");
        run(
            db,
            "?[a,b,c,d] <- [['x','1','yes','yes'],['x','2','no','no']] :put c{a,b=>c,d}",
        );
        for q in [
            "?[b] := a='x', c='yes', *c{a,b,c,d} :reorder written",
            "?[b] := a='x', d='yes', *c{a,b,c,d} :reorder written",
        ] {
            equivalent(db, q, json!([["1"]]));
            assert!(ops(&plan(db, q, true)).contains(&"stored_mat_join"));
        }
        let q = "?[d] := a='x', b='1', d='yes', *c{a,b,c,d} :reorder written";
        equivalent(db, q, json!([["yes"]]));
        assert!(ops(&plan(db, q, true)).contains(&"stored_point_join"));
    });
}

#[test]
fn secondary_prefixes_and_index_to_base_fetch_are_preserved() {
    engines(|db| {
        run(db, ":create s {k:String => name:String,payload:String}");
        run(db, "::index create s:by_name {name}");
        run(
            db,
            "?[k,name,payload] <- [['a','match','p'],['b','miss','q']] :put s{k=>name,payload}",
        );
        for equality in ["=", "=="] {
            for projection in ["k", "k,payload"] {
                let q = format!("?[{projection}] := *s{{k,name,payload}}, name {equality} 'match'");
                let expected = if projection == "k" {
                    json!([["a"]])
                } else {
                    json!([["a", "p"]])
                };
                equivalent(db, &q, expected);
                let p = plan(db, &q, true);
                assert!(refs(&p).contains(&&json!(":s:by_name")), "{p:?}");
                assert!(!ops(&p).contains(&"stored_mat_join"));
            }
        }
    });
}

#[test]
fn local_binding_preserves_other_join_order_and_access() {
    engines(|db| {
        run(db, ":create a {x:String => y:String}");
        run(db, ":create b {y:String => z:String}");
        run(db, ":create c {k:String => z:String}");
        run(db, "?[x,y] <- [['x','y']] :put a{x=>y}");
        run(db, "?[y,z] <- [['y','z']] :put b{y=>z}");
        run(db, "?[k,z] <- [['pick','z']] :put c{k=>z}");
        for suffix in ["", ":reorder written", ":limit 1"] {
            let q = format!("?[x] := *a{{x,y}}, *b{{y,z}}, *c{{k,z}}, k='pick' {suffix}");
            equivalent(db, &q, json!([["x"]]));
            let before = plan(db, &q, false);
            let after = plan(db, &q, true);
            assert_eq!(refs(&before), refs(&after));
            assert!(!ops(&after).contains(&"stored_mat_join"), "{after:?}");
        }
    });
}

#[test]
fn numeric_type_fence_and_empty_input_error_behavior() {
    engines(|db| {
        run(db, ":create n {k:Int}");
        run(
            db,
            "?[k] <- [[5],[9007199254740992],[9007199254740993]] :put n{k}",
        );
        equivalent(db, "?[k] := *n{k}, k=5.0", json!([[5]]));
        equivalent(
            db,
            "?[k] := *n{k}, k=9007199254740992.0",
            json!([[9007199254740992i64], [9007199254740993i64]]),
        );
        assert!(!ops(&plan(db, "?[k] := *n{k}, k=5.0", true)).contains(&"stored_point_join"));
        equivalent(db, "?[k] := *n{k}, k=5", json!([[5]]));
        assert!(ops(&plan(db, "?[k] := *n{k}, k=5", true)).contains(&"stored_point_join"));
        run(db, ":create empty {k:String}");
        equivalent(
            db,
            "?[k] := *empty{k}, k=to_string(assert(false))",
            json!([]),
        );
        for q in ["?[k] := *n{k}, k=rand_int(0,9)", "?[k] := *n{k}, k=now()"] {
            assert_eq!(plan(db, q, false), plan(db, q, true));
        }
    });
}

#[test]
fn temporal_and_negated_access_remain_unchanged() {
    engines(|db| {
        run(db, ":create vt {k:String, at:Validity => v:String}");
        run(
            db,
            "?[k,at,v] <- [['a',[1000000,true],'old'],['a',[2000000,true],'new']] :put vt{k,at=>v}",
        );
        for q in [
            "?[v] := *vt{k,v @ 1500000}, k='a'",
            "?[v] := *vt{k,v}, k='a'",
        ] {
            assert_eq!(plan(db, q, false), plan(db, q, true));
        }
        run(db, ":create tt {k:String, tt:TxTime => v:String}");
        run(db, "?[k,v] <- [['a','new']] :put tt{k=>v}");
        let q = "?[v] := *tt{k,v}, k='a'";
        equivalent(db, q, json!([["new"]]));
        assert_eq!(plan(db, q, false), plan(db, q, true));
        run(
            db,
            ":create bt {k:String, vt:Validity, tt:TxTime => v:String}",
        );
        let q = "?[v] := *bt{k,v @ 'NOW'}, k='a'";
        assert_eq!(plan(db, q, false), plan(db, q, true));
        run(db, ":create other {k:String}");
        let q = "?[k] := k='a', not *other{k}";
        equivalent(db, q, json!([["a"]]));
        assert_eq!(plan(db, q, false), plan(db, q, true));
    });
}

#[test]
fn nullable_any_and_float_keys_preserve_filter_equality() {
    engines(|db| {
        run(db, ":create nullable {k:String?}");
        run(db, "?[k] <- [[null],['a']] :put nullable{k}");
        equivalent(db, "?[k] := *nullable{k}, k=null", json!([[null]]));
        run(db, ":create untyped {k:Any}");
        run(db, "?[k] <- [[5],[5.0]] :put untyped{k}");
        equivalent(db, "?[count(k)] := *untyped{k}, k=5", json!([[2]]));
        run(db, ":create floats {k:Float}");
        run(
            db,
            "?[k] <- [[0.0],[-0.0],[to_float('NaN')],[5.0]] :put floats{k}",
        );
        for expr in ["0.0", "-0.0", "to_float('NaN')", "5.0", "5"] {
            let q = format!("?[k] := *floats{{k}}, k={expr}");
            db.set_keyed_access_for_tests(false);
            let expected = run(db, &q).rows;
            db.set_keyed_access_for_tests(true);
            assert_eq!(run(db, &q).rows, expected, "{q}");
        }
    });
}

#[test]
fn a_partial_primary_prefix_cannot_displace_a_secondary_seek() {
    engines(|db| {
        run(
            db,
            ":create s {a:String,b:String => name:String,payload:String}",
        );
        run(db, "::index create s:by_name {name}");
        run(
            db,
            "?[a,b,name,payload] <- [['x','y','match','p']] :put s{a,b=>name,payload}",
        );
        let q = "?[b,payload] := name='match', *s{a,b,name,payload}, a='x' :reorder written";
        equivalent(db, q, json!([["y", "p"]]));
        assert_eq!(plan(db, q, false), plan(db, q, true));
        let q = "?[payload] := name='match', *s{a,b,name,payload}, a='x', b='y' :reorder written";
        equivalent(db, q, json!([["p"]]));
        let p = plan(db, q, true);
        assert!(!refs(&p).contains(&&json!(":s:by_name")));
        assert!(ops(&p).contains(&"stored_point_join"));
    });
}

#[test]
fn volatile_filters_keep_their_baseline_evaluation_location() {
    engines(|db| {
        run(db, ":create s {k:String => name:String}");
        run(db, "::index create s:by_name {name}");
        run(db, "?[k,name] <- [['x','a'],['y','a']] :put s{k=>name}");
        let q = "?[k] := *s{k,name}, name=rand_choose(['a','b']), name='a'";
        assert_eq!(plan(db, q, false), plan(db, q, true));
        run(db, ":create n {id:String => a:String,b:String}");
        run(db, "?[id,a,b] <- [['x','gap','yes']] :put n{id=>a,b}");
        let q = "?[count(id)] := z in [1,1], id='x', b='yes', *n{id,a,b}, a=rand_choose(['gap','other']) :reorder written";
        assert_eq!(plan(db, q, false), plan(db, q, true));
    });
}

#[test]
fn vector_equality_is_not_replaced_by_bitwise_key_identity() {
    engines(|db| {
        run(db, ":create v {k:<F32;1> => a:String,label:String}");
        run(
            db,
            "?[k,a,label] <- [[vec([-0.0]),'gap','hit']] :put v{k=>a,label}",
        );
        let q = "?[label] := *v{k,label}, k=vec([0.0])";
        equivalent(db, q, json!([["hit"]]));
        assert_eq!(plan(db, q, false), plan(db, q, true));
        let q = "?[label] := k=vec([0.0]), label='hit', *v{k,a,label} :reorder written";
        equivalent(db, q, json!([["hit"]]));
        assert_eq!(plan(db, q, false), plan(db, q, true));
    });
}

#[test]
fn new_bindings_do_not_evaluate_fallible_filters_before_an_empty_lookup() {
    engines(|db| {
        run(db, ":create empty {k:String}");
        for q in [
            "?[k] := *empty{k}, k='a', k in 5 :reorder written",
            "?[k] := source=5, *empty{k}, k='a', k in source :reorder written",
        ] {
            equivalent(db, q, json!([]));
            assert_eq!(plan(db, q, false), plan(db, q, true));
        }
        let q = "?[k] := *empty{k}, k='a', assert(k=='b')";
        equivalent(db, q, json!([]));
        assert_eq!(plan(db, q, false), plan(db, q, true));
    });
}

#[test]
fn fallible_right_filters_are_not_skipped_on_nonmatching_rows() {
    engines(|db| {
        run(db, ":create n {id:String => a:String,b:String}");
        run(
            db,
            "?[id,a,b] <- [['x','1','yes'],['z','invalid','no']] :put n{id=>a,b}",
        );
        for q in [
            "?[id] := id='x', b='yes', *n{id,a,b}, assert(to_int(a)>0) :reorder written",
            "?[id] := *n{id,a,b}, id='x', assert(to_int(a)>0) :reorder written",
        ] {
            for enabled in [false, true] {
                db.set_keyed_access_for_tests(enabled);
                assert!(
                    db.run_script(q, BTreeMap::new(), ScriptMutability::Immutable)
                        .is_err(),
                    "{enabled}: {q}"
                );
            }
            assert_eq!(plan(db, q, false), plan(db, q, true));
        }
    });
}

#[test]
fn a_folded_noncandidate_filter_cannot_turn_a_point_into_materialization() {
    engines(|db| {
        run(db, ":create n {id:String => a:String,b:String}");
        run(db, ":create other {a:String}");
        run(db, "?[id,a,b] <- [['x','gap','yes']] :put n{id=>a,b}");
        run(db, "?[a] <- [['gap']] :put other{a}");
        for q in [
            "?[id] := id='x', *n{id,a,b}, a=concat('g','ap'), b='yes', *other{a} :reorder written",
            "?[id] := id='x', *n{id,a,b}, a in ['gap'], b='yes', *other{a} :reorder written",
        ] {
            equivalent(db, q, json!([["x"]]));
            assert_eq!(plan(db, q, false), plan(db, q, true));
        }
    });
}

#[test]
fn consumer_lint_positive_fixtures_have_keyed_plans_with_declared_string_keys() {
    // Complete the five positive query snippets in MindGraph's
    // scripts/fixtures/cozo-query-lint/cases.rs with explicit schema/params.
    engines(|db| {
        run(
            db,
            ":create node {uid:String => node_type:String,tombstone_at:Float}",
        );
        run(db, "?[uid,node_type,tombstone_at] <- [['u','memory',0.0],['v','memory',1.0]] :put node{uid=>node_type,tombstone_at}");
        run(db, ":create edge {uid:String => a:String}");
        run(db, "?[uid,a] <- [['u','v']] :put edge{uid=>a}");
        run(db, ":create adjacency {from_uid:String,to_uid:String}");
        run(
            db,
            "?[from_uid,to_uid] <- [['u','v']] :put adjacency{from_uid,to_uid}",
        );
        // The positional node fixture has a two-column schema in the lint;
        // append an anonymous column here because this shared fixture also has tombstone_at.
        for (query, expected) in [
            ("?[uid] := *node[uid,node_type,_], uid == 'u'", json!([["u"]])),
            ("frontier[s] <- [['u']] ?[t] := frontier[s], *adjacency{from_uid,to_uid:t}, from_uid == s", json!([["v"]])),
            ("?[uid] := *node{uid}, uid = 'u' :limit 1", json!([["u"]])),
            ("?[uid] := *node{uid,tombstone_at}, uid = 'u', tombstone_at == 0.0", json!([["u"]])),
            ("?[a] := *edge[uid,a], uid = 'u'", json!([["v"]])),
        ] {
            equivalent(db, query, expected);
            let optimized = plan(db, query, true);
            assert!(ops(&optimized).iter().any(|op| matches!(*op, "stored_point_join" | "stored_prefix_join")), "{query}: {optimized:?}");
            assert!(!ops(&optimized).contains(&"materialized_join"), "{query}: {optimized:?}");
        }
    });
}
