/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Stored / named query regressions. SQLite is intentional: the feature reads
//! its catalog through the persistent stored-relation path.

use std::collections::BTreeMap;

use cozo::{DataValue, DbInstance, ScriptMutability, SimpleFixedRule};

fn sqlite(path: &std::path::Path) -> DbInstance {
    DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap()
}

fn run(db: &DbInstance, script: &str) -> cozo::NamedRows {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap()
}

fn err(db: &DbInstance, script: &str, params: BTreeMap<String, DataValue>) -> String {
    format!(
        "{:?}",
        db.run_script(script, params, ScriptMutability::Mutable)
            .unwrap_err()
    )
}

#[test]
fn stored_query_create_list_show_atom_run_and_remove() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite(&dir.path().join("basic.db"));
    run(&db, ":create items {id: Int => label: String}");
    run(
        &db,
        "?[id, label] <- [[1, 'one'], [2, 'two']] :put items {id => label}",
    );

    run(
        &db,
        "::query create item_labels { ?[id, label] := *items[id, label] }",
    );
    let listed = run(&db, "::query list");
    assert_eq!(listed.rows.len(), 1);
    assert_eq!(listed.rows[0][0].get_str(), Some("item_labels"));
    let shown = run(&db, "::query show item_labels");
    assert!(shown.rows[0][1]
        .get_str()
        .unwrap()
        .contains("*items[id, label]"));

    let atom = run(&db, "?[label] := item_labels[2, label]");
    assert_eq!(atom.rows, vec![vec![DataValue::from("two")]]);
    let standalone = run(&db, "::query run item_labels");
    assert_eq!(standalone.rows.len(), 2);

    run(&db, "::query remove item_labels");
    assert!(run(&db, "::query list").rows.is_empty());
}

#[test]
fn stored_query_params_are_declared_defaulted_and_coerced() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite(&dir.path().join("params.db"));
    run(&db, ":create items {id: Int => label: String}");
    run(
        &db,
        "?[id, label] <- [[1, 'one'], [2, 'two']] :put items {id => label}",
    );
    run(
        &db,
        "::query create by_id ($id: Int default 1) { ?[label] := *items[$id, label] }",
    );
    assert_eq!(
        run(&db, "::query run by_id").rows,
        vec![vec![DataValue::from("one")]]
    );

    let mut params = BTreeMap::new();
    params.insert("id".to_string(), DataValue::from(2.0));
    let coerced = db
        .run_script("::query run by_id", params, ScriptMutability::Immutable)
        .unwrap();
    assert_eq!(coerced.rows, vec![vec![DataValue::from("two")]]);

    run(
        &db,
        "::query create required ($id: Int) { ?[label] := *items[$id, label] }",
    );
    let missing = err(&db, "::query run required", BTreeMap::new());
    assert!(missing.contains("requires parameter '$id'"), "{missing}");

    let undeclared = err(
        &db,
        "::query create bad { ?[x] := x = $missing }",
        BTreeMap::new(),
    );
    assert!(undeclared.contains("undeclared parameter"), "{undeclared}");
    let unused = err(
        &db,
        "::query create stale ($unused: Int) { ?[x] <- [[1]] }",
        BTreeMap::new(),
    );
    assert!(unused.contains("unused parameter"), "{unused}");
}

#[test]
fn stored_query_dependencies_are_hygienic_idempotent_and_restricted() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite(&dir.path().join("deps.db"));
    run(&db, "::query create leaf { ?[x] <- [['stored']] }");
    run(&db, "::query create left { ?[x] := leaf[x] }");
    run(&db, "::query create right { ?[x] := leaf[x] }");
    run(
        &db,
        "::query create diamond { ?[x] := left[x] or right[x] }",
    );
    assert_eq!(
        run(&db, "?[x] := diamond[x]").rows,
        vec![vec![DataValue::from("stored")]],
        "a diamond dependency must splice the shared leaf once"
    );

    let hygienic = run(&db, "?[x] := diamond[x]; leaf[x] <- [['caller-local']]");
    assert_eq!(hygienic.rows, vec![vec![DataValue::from("stored")]]);

    let local = run(&db, "?[x] := leaf[x]; leaf[x] <- [['caller-local']]");
    assert_eq!(local.rows, vec![vec![DataValue::from("caller-local")]]);
    let warnings = run(&db, "::warnings");
    assert!(warnings.rows.iter().any(|row| {
        row.get(1).and_then(DataValue::get_str) == Some("stored_query.local_shadow")
    }));

    let depended = err(&db, "::query remove leaf", BTreeMap::new());
    assert!(depended.contains("depends on it"), "{depended}");
    let missing = err(
        &db,
        "::query create broken { ?[x] := does_not_exist[x] }",
        BTreeMap::new(),
    );
    assert!(
        missing.contains("missing rule or stored query"),
        "{missing}"
    );
}

#[test]
fn stored_query_atom_gates_options_arity_and_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite(&dir.path().join("gates.db"));
    run(
        &db,
        "::query create limited { ?[x] <- [[1], [2], [3]] :limit 1 }",
    );
    let atom_error = err(&db, "?[x] := limited[x]", BTreeMap::new());
    assert!(atom_error.contains(":limit"), "{atom_error}");
    assert_eq!(run(&db, "::query run limited").rows.len(), 1);
    db.set_default_query_mem_limit(Some(1));
    let budgeted = err(&db, "::query run limited", BTreeMap::new());
    assert!(budgeted.contains("mem_budget_exceeded"), "{budgeted}");
    db.set_default_query_mem_limit(None);

    run(&db, "::query create pair { ?[a, b] <- [[1, 2]] }");
    let arity = err(&db, "?[a] := pair[a]", BTreeMap::new());
    assert!(arity.contains("arity mismatch"), "{arity}");

    let mutation = err(
        &db,
        "::query create writer { ?[x] <- [[1]] :create forbidden {x} }",
        BTreeMap::new(),
    );
    assert!(mutation.contains("must be read-only"), "{mutation}");
}

#[test]
fn stored_query_persists_and_supports_explain_aggregation_and_negation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("persistent.db");
    {
        let db = sqlite(&path);
        run(&db, "::query create nums { ?[x] <- [[1], [2], [3]] }");
    }
    let db = sqlite(&path);
    assert_eq!(run(&db, "::query run nums").rows.len(), 3);

    let count = run(&db, "?[count(x)] := nums[x]");
    assert_eq!(count.rows, vec![vec![DataValue::from(3_i64)]]);
    let negated = run(
        &db,
        "candidate[x] <- [[1], [4]]; ?[x] := candidate[x], not nums[x]",
    );
    assert_eq!(negated.rows, vec![vec![DataValue::from(4_i64)]]);

    let explain = run(&db, "::explain { ?[x] := nums[x] }");
    assert!(
        explain
            .rows
            .iter()
            .any(|row| format!("{row:?}").contains("nums::?")),
        "explain should expose the hygienically mangled stored-query entry"
    );
}

#[test]
fn stored_query_works_in_imperative_scripts_and_defaulted_triggers() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite(&dir.path().join("embedded.db"));
    let imperative = run(
        &db,
        "{::query create one { ?[x] <- [[1]] }} { ?[x] := one[x] }",
    );
    assert_eq!(imperative.rows, vec![vec![DataValue::from(1_i64)]]);

    run(&db, ":create source {k: Int => ignored: Int}");
    run(&db, ":create audit {k: Int => v: Int}");
    run(
        &db,
        "::query create default_value ($value: Int default 7) { ?[v] := v = $value }",
    );
    run(
        &db,
        "::set_triggers source on put { \
         ?[k, v] := _new[k, ignored], default_value[v] \
         :put audit {k => v} }",
    );
    run(&db, "?[k, ignored] <- [[1, 99]] :put source {k => ignored}");
    assert_eq!(
        run(&db, "?[k, v] := *audit[k, v]").rows,
        vec![vec![DataValue::from(1_i64), DataValue::from(7_i64)]]
    );
}

#[test]
fn stored_query_catalog_obeys_access_levels_and_round_trips_export() {
    let dir = tempfile::tempdir().unwrap();
    let source_path = dir.path().join("export-source.db");
    let source = sqlite(&source_path);
    run(&source, "::query create exported { ?[x] <- [[42]] }");
    let dump = source
        .export_relations(["mnestic_stored_queries"].into_iter())
        .unwrap();

    let destination = sqlite(&dir.path().join("export-destination.db"));
    run(&destination, "::query create placeholder { ?[x] <- [[0]] }");
    destination.import_relations(dump).unwrap();
    assert_eq!(
        run(&destination, "::query run exported").rows,
        vec![vec![DataValue::from(42_i64)]]
    );

    run(
        &destination,
        "::access_level read_only mnestic_stored_queries",
    );
    assert_eq!(run(&destination, "::query run exported").rows.len(), 1);
    let frozen = err(
        &destination,
        "::query create blocked { ?[x] <- [[1]] }",
        BTreeMap::new(),
    );
    assert!(frozen.contains("read_only"), "{frozen}");
}

#[test]
fn stored_query_defends_against_hand_edited_cycles_and_writes() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite(&dir.path().join("tampered.db"));
    run(&db, "::query create cycle { ?[x] <- [[1]] }");
    run(
        &db,
        "?[name, body] <- [['cycle', '?[x] := cycle[x]']] \
         :update mnestic_stored_queries {name => body}",
    );
    let cycle = err(&db, "::query run cycle", BTreeMap::new());
    assert!(cycle.contains("exceeds depth 32"), "{cycle}");

    run(&db, "::query create writer { ?[x] <- [[1]] }");
    run(
        &db,
        "?[name, body] <- [['writer', '?[x] <- [[1]] :create illicit {x}']] \
         :update mnestic_stored_queries {name => body}",
    );
    let writer = err(&db, "::query run writer", BTreeMap::new());
    assert!(writer.contains("not read-only"), "{writer}");
}

#[test]
fn stored_query_reparses_fixed_rules_against_the_live_registry() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite(&dir.path().join("fixed.db"));
    db.register_fixed_rule(
        "StoredQueryProbe".to_string(),
        SimpleFixedRule::new(1, |_inputs, _options| {
            Ok(cozo::NamedRows::new(
                vec!["x".to_string()],
                vec![vec![DataValue::from(1_i64)]],
            ))
        }),
    )
    .unwrap();
    run(
        &db,
        "::query create fixed_probe { ?[x] <~ StoredQueryProbe() }",
    );
    assert!(db.unregister_fixed_rule("StoredQueryProbe").unwrap());
    let unregistered = err(&db, "::query run fixed_probe", BTreeMap::new());
    assert!(
        unregistered.contains("StoredQueryProbe") && unregistered.contains("fixed rule"),
        "{unregistered}"
    );
}
