/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Translator + run_cypher tests: golden translations, end-to-end execution
//! against an in-memory DB (both schema conventions), and regression tests for
//! the adversarial-review findings (docs/specs/cypher-read-review-findings.json):
//! null/3VL WHERE, injective prop vars, aggregate nulls, duplicate id binding,
//! edge-isomorphism, bag fidelity, binding validation, param-namespace safety.

use super::*;
use crate::cypher::parse::parse_cypher;
use crate::{DataValue, DbInstance, ScriptMutability};
use std::collections::BTreeMap;

fn tr(q: &str, schema: &CypherGraphSchema) -> CypherScript {
    cypher_to_script(&parse_cypher(q).unwrap(), schema).unwrap()
}

fn nm(label: &str, relation: &str, id_col: &str, label_col: Option<&str>) -> NodeMap {
    NodeMap {
        label: label.into(),
        relation: relation.into(),
        id_col: id_col.into(),
        label_col: label_col.map(|s| s.into()),
        label_value: None,
        filter: None,
    }
}

fn relation_per_label() -> CypherGraphSchema {
    CypherGraphSchema {
        nodes: vec![nm("Person", "person", "id", None)],
        edges: vec![EdgeMap {
            rel_type: "KNOWS".into(),
            relation: "knows".into(),
            from_col: "fr".into(),
            to_col: "to".into(),
            type_col: None,
            type_value: None,
            eid_col: None,
            filter: None,
        }],
    }
}

fn shared_relation() -> CypherGraphSchema {
    CypherGraphSchema {
        nodes: vec![nm("Person", "node", "uid", Some("node_type"))],
        edges: vec![EdgeMap {
            rel_type: "KNOWS".into(),
            relation: "edge".into(),
            from_col: "from_uid".into(),
            to_col: "to_uid".into(),
            type_col: Some("edge_type".into()),
            type_value: None,
            eid_col: Some("uid".into()),
            filter: None,
        }],
    }
}

/// Canonical relation-per-label fixture: person (1,Alice,30),(2,Bob,40),(3,Carol,35);
/// knows (1,2),(1,3),(2,3).
fn canonical() -> DbInstance {
    let db = DbInstance::default();
    db.run_default(":create person {id => name, age}").unwrap();
    db.run_default(":create knows {fr, to}").unwrap();
    db.run_default(
        "?[id, name, age] <- [[1,'Alice',30],[2,'Bob',40],[3,'Carol',35]] :put person {id => name, age}",
    )
    .unwrap();
    db.run_default("?[fr, to] <- [[1,2],[1,3],[2,3]] :put knows {fr, to}").unwrap();
    db
}

fn run(db: &DbInstance, cs: &CypherScript) -> Vec<Vec<DataValue>> {
    let out = db
        .run_script(&cs.script, cs.params.clone(), ScriptMutability::Immutable)
        .unwrap_or_else(|e| panic!("script failed:\n{}\nerror: {e}", cs.script));
    let n = cs.out_columns.len();
    out.rows.into_iter().map(|r| r[..n].to_vec()).collect()
}

fn s(x: &str) -> DataValue {
    DataValue::from(x)
}
fn i(x: i64) -> DataValue {
    DataValue::from(x)
}

// --- golden translations ---

#[test]
fn golden_relation_per_label() {
    let cs = tr(
        "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.age > 30 RETURN b.name AS name ORDER BY name",
        &relation_per_label(),
    );
    assert!(cs.script.contains("*person{"), "{}", cs.script);
    assert!(cs.script.contains("*knows{fr: a, to: b}"), "{}", cs.script);
    assert!(cs.script.contains(":order +cy_ret_0"), "{}", cs.script);
    assert_eq!(cs.out_columns, vec!["name"]);
    assert_eq!(cs.params.len(), 1); // the literal 30
}

#[test]
fn golden_prop_vars_are_injective_opaque() {
    // M2 regression: property vars must be opaque cyp_N, never cyp_<var>_<key>
    // (which collides, e.g. (a,'b_c') vs (a_b,'c')).
    let cs = tr("MATCH (a:Person) RETURN a.name, a.age", &relation_per_label());
    assert!(cs.script.contains("cyp_0"), "{}", cs.script);
    assert!(cs.script.contains("cyp_1"), "{}", cs.script);
    assert!(!cs.script.contains("cyp_a_"), "non-injective naming leaked:\n{}", cs.script);
}

#[test]
fn golden_shared_relation_eid_and_iso() {
    let cs = tr(
        "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) RETURN c.name",
        &shared_relation(),
    );
    assert!(cs.script.contains("node_type: $cphr"), "{}", cs.script);
    assert!(cs.script.contains("edge_type: $cphr"), "{}", cs.script);
    assert!(cs.script.contains("uid:"), "{}", cs.script);
    assert!(cs.script.contains(" != "), "missing eid iso disequality:\n{}", cs.script);
}

// --- execution: core ---

#[test]
fn exec_where_order() {
    let db = canonical();
    let cs = tr(
        "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.age > 30 RETURN b.name AS name ORDER BY name",
        &relation_per_label(),
    );
    assert_eq!(run(&db, &cs), vec![vec![s("Carol")]]);
}

#[test]
fn exec_two_hop_chain() {
    let db = canonical();
    let cs = tr(
        "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) RETURN a.name AS a, b.name AS b, c.name AS c",
        &relation_per_label(),
    );
    assert_eq!(run(&db, &cs), vec![vec![s("Alice"), s("Bob"), s("Carol")]]);
}

#[test]
fn exec_bag_vs_distinct_and_count() {
    let db = canonical();
    let sch = relation_per_label();
    // bag: Alice,Alice,Bob (3 rows)
    assert_eq!(run(&db, &tr("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name", &sch)).len(), 3);
    // distinct: Alice,Bob (2)
    assert_eq!(run(&db, &tr("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN DISTINCT a.name", &sch)).len(), 2);
    // count(*) grouped
    assert_eq!(
        run(&db, &tr("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name AS who, count(*) AS c ORDER BY who", &sch)),
        vec![vec![s("Alice"), i(2)], vec![s("Bob"), i(1)]]
    );
    // count(*) ungrouped
    assert_eq!(run(&db, &tr("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN count(*) AS c", &sch)), vec![vec![i(3)]]);
}

#[test]
fn exec_aggregates_min_max_sum_avg_collect() {
    let db = canonical();
    let sch = relation_per_label();
    // min/max preserve integer type
    assert_eq!(run(&db, &tr("MATCH (a:Person) RETURN min(a.age) AS lo, max(a.age) AS hi", &sch)), vec![vec![i(30), i(40)]]);
    // sum: KNOWN DIVERGENCE — engine accumulator is f64, so an integer column sums to a float.
    assert_eq!(run(&db, &tr("MATCH (a:Person) RETURN sum(a.age) AS t", &sch)), vec![vec![DataValue::from(105.0f64)]]);
    // avg -> mean (float)
    assert_eq!(run(&db, &tr("MATCH (a:Person) RETURN avg(a.age) AS m", &sch)), vec![vec![DataValue::from(35.0f64)]]);
    // collect grouped: Alice -> 2 friends, Bob -> 1 (order unspecified)
    let rows = run(&db, &tr("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name AS who, collect(b.name) AS friends ORDER BY who", &sch));
    assert_eq!(rows.len(), 2);
    if let DataValue::List(v) = &rows[0][1] { assert_eq!(v.len(), 2); } else { panic!("Alice friends not a list: {:?}", rows[0][1]); }
    if let DataValue::List(v) = &rows[1][1] { assert_eq!(v.len(), 1); } else { panic!("Bob friends not a list"); }
}

#[test]
fn exec_order_skip_limit_pagination() {
    let db = canonical();
    let cs = tr("MATCH (a:Person) RETURN a.name AS name, a.age AS age ORDER BY age DESC SKIP 1 LIMIT 1", &relation_per_label());
    assert_eq!(run(&db, &cs), vec![vec![s("Carol"), i(35)]]);
}

#[test]
fn exec_limit_counts_bag_rows() {
    let db = canonical();
    // bag ordered by name = Alice,Alice,Bob; LIMIT 2 keeps both Alices.
    let cs = tr("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name AS name ORDER BY name LIMIT 2", &relation_per_label());
    assert_eq!(run(&db, &cs), vec![vec![s("Alice")], vec![s("Alice")]]);
}

#[test]
fn exec_order_by_aggregate_alias_and_bare_expr() {
    let db = canonical();
    let sch = relation_per_label();
    // ORDER BY aggregate alias
    assert_eq!(
        run(&db, &tr("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name AS who, count(*) AS c ORDER BY c DESC", &sch)),
        vec![vec![s("Alice"), i(2)], vec![s("Bob"), i(1)]]
    );
    // N1: ORDER BY an unaliased aggregate expression already in RETURN
    assert_eq!(
        run(&db, &tr("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name AS who, count(*) ORDER BY count(*) DESC", &sch)),
        vec![vec![s("Alice"), i(2)], vec![s("Bob"), i(1)]]
    );
}

#[test]
fn exec_where_in_string_and_boolean() {
    let db = canonical();
    let sch = relation_per_label();
    assert_eq!(run(&db, &tr("MATCH (a:Person) WHERE a.age IN [30, 40] RETURN a.name AS name ORDER BY name", &sch)), vec![vec![s("Alice")], vec![s("Bob")]]);
    assert_eq!(run(&db, &tr("MATCH (a:Person) WHERE a.name STARTS WITH 'A' RETURN a.name AS name", &sch)), vec![vec![s("Alice")]]);
    assert_eq!(run(&db, &tr("MATCH (a:Person) WHERE a.name CONTAINS 'o' RETURN a.name AS name ORDER BY name", &sch)), vec![vec![s("Bob")], vec![s("Carol")]]);
    assert_eq!(run(&db, &tr("MATCH (a:Person) WHERE a.age > 30 AND a.age < 40 RETURN a.name AS name", &sch)), vec![vec![s("Carol")]]);
}

// --- M1: null-aware WHERE (3VL) ---

fn with_null_age() -> DbInstance {
    let db = DbInstance::default();
    db.run_default(":create person {id => name, age}").unwrap();
    db.run_default("?[id, name, age] <- [[1,'Alice',30],[2,'Bob',40],[3,'Dave',null]] :put person {id => name, age}").unwrap();
    db
}

#[test]
fn exec_where_comparison_excludes_null_not_errors() {
    // The headline review finding: a null operand must DROP the row, not abort.
    let db = with_null_age();
    let sch = relation_per_label();
    assert_eq!(run(&db, &tr("MATCH (a:Person) WHERE a.age >= 40 RETURN a.name AS name", &sch)), vec![vec![s("Bob")]]);
    // NOT over a comparison with a null operand also excludes the null row.
    assert_eq!(run(&db, &tr("MATCH (a:Person) WHERE NOT a.age >= 40 RETURN a.name AS name ORDER BY name", &sch)), vec![vec![s("Alice")]]);
}

#[test]
fn exec_where_is_null_is_not_null() {
    let db = with_null_age();
    let sch = relation_per_label();
    assert_eq!(run(&db, &tr("MATCH (a:Person) WHERE a.age IS NULL RETURN a.name AS name", &sch)), vec![vec![s("Dave")]]);
    assert_eq!(run(&db, &tr("MATCH (a:Person) WHERE a.age IS NOT NULL RETURN a.name AS name ORDER BY name", &sch)), vec![vec![s("Alice")], vec![s("Bob")]]);
}

#[test]
fn exec_aggregate_skips_nulls_not_errors() {
    // M3: sum/avg must skip null cells instead of aborting; count(prop) skips nulls.
    let db = with_null_age(); // ages 30, 40, null
    let sch = relation_per_label();
    assert_eq!(run(&db, &tr("MATCH (a:Person) RETURN sum(a.age) AS t", &sch)), vec![vec![DataValue::from(70.0f64)]]);
    assert_eq!(run(&db, &tr("MATCH (a:Person) RETURN count(a.age) AS c", &sch)), vec![vec![i(2)]]);
    // count(*) still counts all rows including the null one.
    assert_eq!(run(&db, &tr("MATCH (a:Person) RETURN count(*) AS c", &sch)), vec![vec![i(3)]]);
}

// --- M4: id-column reference no longer double-binds ---

#[test]
fn exec_return_and_filter_on_id_column() {
    let db = canonical();
    let sch = relation_per_label();
    assert_eq!(run(&db, &tr("MATCH (a:Person) WHERE a.id = 1 RETURN a.id AS id", &sch)), vec![vec![i(1)]]);
    // inline id constraint
    assert_eq!(run(&db, &tr("MATCH (a:Person {id: 2}) RETURN a.name AS name", &sch)), vec![vec![s("Bob")]]);
}

// --- edge-isomorphism ---

#[test]
fn exec_edge_iso_self_loop_excludes_reuse() {
    let db = DbInstance::default();
    db.run_default(":create person {id => name}").unwrap();
    db.run_default(":create knows {fr, to}").unwrap();
    db.run_default("?[id, name] <- [[1,'Alice'],[2,'Bob']] :put person {id => name}").unwrap();
    db.run_default("?[fr, to] <- [[1,1],[1,2]] :put knows {fr, to}").unwrap(); // self-loop 1->1
    let cs = tr("MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) RETURN a.name AS a, b.name AS b, c.name AS c", &relation_per_label());
    // (1,1,1) must be excluded (same edge reused); only (1,1,2) survives.
    assert_eq!(run(&db, &cs), vec![vec![s("Alice"), s("Alice"), s("Bob")]]);
}

#[test]
fn exec_multi_match_no_cross_clause_iso() {
    let db = canonical();
    let cs = tr(
        "MATCH (a:Person)-[:KNOWS]->(b:Person) MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name AS a, b.name AS b ORDER BY a, b",
        &relation_per_label(),
    );
    // edge-iso is per-MATCH: the same edge satisfies both clauses -> 3 rows, not 0.
    assert_eq!(run(&db, &cs).len(), 3);
}

#[test]
fn exec_shared_relation_parallel_edges() {
    let db = DbInstance::default();
    db.run_default(":create node {uid => node_type, name}").unwrap();
    db.run_default(":create edge {uid => from_uid, to_uid, edge_type}").unwrap();
    db.run_default("?[uid, node_type, name] <- [['n1','Person','Alice'],['n2','Person','Bob']] :put node {uid => node_type, name}").unwrap();
    db.run_default("?[uid, from_uid, to_uid, edge_type] <- [['e1','n1','n2','KNOWS'],['e2','n1','n2','KNOWS']] :put edge {uid => from_uid, to_uid, edge_type}").unwrap();
    // two parallel KNOWS edges -> 2 bindings (eid in the binding key) -> 2 rows.
    let cs = tr("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name AS name", &shared_relation());
    assert_eq!(run(&db, &cs), vec![vec![s("Alice")], vec![s("Alice")]]);
}

#[test]
fn exec_relationship_property_filter() {
    let db = DbInstance::default();
    db.run_default(":create person {id => name}").unwrap();
    db.run_default(":create knows {fr, to => since}").unwrap();
    db.run_default("?[id, name] <- [[1,'Alice'],[2,'Bob'],[3,'Carol']] :put person {id => name}").unwrap();
    db.run_default("?[fr, to, since] <- [[1,2,2001],[1,3,1999],[2,3,2005]] :put knows {fr, to => since}").unwrap();
    let cs = tr("MATCH (a:Person)-[r:KNOWS]->(b:Person) WHERE r.since >= 2001 RETURN a.name AS a, b.name AS b ORDER BY a, b", &relation_per_label());
    assert_eq!(run(&db, &cs), vec![vec![s("Alice"), s("Bob")], vec![s("Bob"), s("Carol")]]);
}

// --- run_cypher entry + projection ---

#[test]
fn run_cypher_entry_projects_and_names_columns() {
    let db = canonical();
    let out = db
        .run_cypher(
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.age > 30 RETURN b.name AS name ORDER BY name",
            &relation_per_label(),
            BTreeMap::new(),
        )
        .unwrap();
    assert_eq!(out.headers, vec!["name"]);
    assert_eq!(out.rows, vec![vec![s("Carol")]]);
}

#[test]
fn run_cypher_passes_user_params() {
    let db = canonical();
    let mut params = BTreeMap::new();
    params.insert("minAge".to_string(), i(30));
    let out = db
        .run_cypher("MATCH (a:Person) WHERE a.age > $minAge RETURN a.name AS name ORDER BY name", &relation_per_label(), params)
        .unwrap();
    assert_eq!(out.rows, vec![vec![s("Bob")], vec![s("Carol")]]);
}

#[test]
fn run_cypher_rejects_reserved_user_param() {
    // S2: a user param colliding with the generated cphr_ namespace is rejected.
    let db = canonical();
    let mut params = BTreeMap::new();
    params.insert("cphr_0".to_string(), i(1));
    assert!(db
        .run_cypher("MATCH (a:Person) RETURN a.name", &relation_per_label(), params)
        .is_err());
}

#[test]
fn shared_relation_mindgraph_end_to_end() {
    let db = DbInstance::default();
    db.run_default(":create node {uid => node_type, name, age}").unwrap();
    db.run_default(":create edge {uid => from_uid, to_uid, edge_type}").unwrap();
    db.run_default("?[uid, node_type, name, age] <- [['n1','Person','Alice',30],['n2','Person','Bob',40]] :put node {uid => node_type, name, age}").unwrap();
    db.run_default("?[uid, from_uid, to_uid, edge_type] <- [['e1','n1','n2','KNOWS']] :put edge {uid => from_uid, to_uid, edge_type}").unwrap();
    let out = db.run_cypher("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN b.name AS name", &shared_relation(), BTreeMap::new()).unwrap();
    assert_eq!(out.rows, vec![vec![s("Bob")]]);
}

// --- error paths (binding validation, var reuse, reserved names, deferred features) ---

#[test]
fn errors_are_clear_not_opaque() {
    let s = relation_per_label();
    let p = |q: &str| cypher_to_script(&parse_cypher(q).unwrap(), &s);
    // bare relationship variable with no eid_col (S5)
    assert!(p("MATCH (a:Person)-[r:KNOWS]->(b:Person) RETURN r").is_err());
    // undefined variable
    assert!(p("MATCH (a:Person) RETURN b.name").is_err());
    // reused relationship variable (S4)
    assert!(p("MATCH (a:Person)-[r:KNOWS]->(b:Person)-[r:KNOWS]->(c:Person) RETURN a").is_err());
    // name used as both node and relationship
    assert!(p("MATCH (x:Person)-[x:KNOWS]->(b:Person) RETURN b").is_err());
    // reserved generated name (S1)
    assert!(p("MATCH (cy_ret_0:Person) RETURN cy_ret_0").is_err());
    // undirected (deferred), unknown label, schema filter (deferred)
    assert!(p("MATCH (a:Person)-[:KNOWS]-(b:Person) RETURN a").is_err());
    assert!(p("MATCH (a:Ghost) RETURN a").is_err());
    let mut s2 = relation_per_label();
    s2.nodes[0].filter = Some("age > 0".into());
    assert!(cypher_to_script(&parse_cypher("MATCH (a:Person) RETURN a").unwrap(), &s2).is_err());
}

#[test]
fn unlabeled_node_over_single_shared_relation() {
    // shared-relation convention allows an unlabeled node (drops the discriminator).
    let db = DbInstance::default();
    db.run_default(":create node {uid => node_type, name}").unwrap();
    db.run_default("?[uid, node_type, name] <- [['n1','Person','Alice']] :put node {uid => node_type, name}").unwrap();
    let mut sch = shared_relation();
    // a single node mapping shares one relation+id_col, so (a) with no label is allowed.
    let _ = &mut sch;
    let out = run(&db, &tr("MATCH (a) RETURN a.name AS name", &sch));
    assert_eq!(out, vec![vec![s("Alice")]]);
}

#[test]
fn cypher_to_script_is_inspectable() {
    let db = DbInstance::default();
    let (script, params) = db
        .cypher_to_script("MATCH (a:Person) WHERE a.age > 30 RETURN a.name", &relation_per_label())
        .unwrap();
    assert!(script.contains("*person{"), "{script}");
    assert_eq!(params.len(), 1);
}
