/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Executable guard for `docs/guides/modeling-tree-shaped-data.md`.

use cozo::DbInstance;

fn run(db: &DbInstance, script: &str) -> serde_json::Value {
    db.run_default(script)
        .unwrap_or_else(|err| panic!("guide script failed:\n{script}\n{err:?}"))
        .into_json()
}

#[test]
fn tree_modeling_guide_is_runnable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tree-modeling-guide.db");
    let db = DbInstance::new("sqlite", path.to_str().unwrap(), Default::default()).unwrap();

    run(&db, ":create raw_document {doc_id: String => body: Json}");
    run(
        &db,
        r#"?[doc_id, body] <- [[
          'person:ada',
          parse_json('{"@id":"person:ada","@type":"Person","profile":{"name":"Ada Lovelace","address":{"street":"12 St James Square","city":"London"}},"interests":["mathematics","poetry"]}')
        ]]
        :put raw_document {doc_id => body}"#,
    );

    let raw = run(
        &db,
        "?[doc_id, name, city] := *raw_document{doc_id, body}, name = get(body, ['profile', 'name']), city = get(body, ['profile', 'address', 'city'])",
    );
    assert_eq!(
        raw["rows"],
        serde_json::json!([["person:ada", "Ada Lovelace", "London"]])
    );

    run(&db, ":create person {person_id: String => name: String}");
    run(
        &db,
        ":create address {address_id: String => person_id: String, street: String, city: String}",
    );
    run(
        &db,
        "?[person_id, name] := *raw_document{doc_id: person_id, body}, name = get(body, ['profile', 'name']) :put person {person_id => name}",
    );
    run(
        &db,
        "?[address_id, person_id, street, city] := *raw_document{doc_id: person_id, body}, address_id = concat(person_id, ':address'), street = get(body, ['profile', 'address', 'street']), city = get(body, ['profile', 'address', 'city']) :put address {address_id => person_id, street, city}",
    );
    let joined = run(
        &db,
        "?[name, city] := *person{person_id, name}, *address{person_id, city}",
    );
    assert_eq!(
        joined["rows"],
        serde_json::json!([["Ada Lovelace", "London"]])
    );

    run(
        &db,
        ":create person_interest {person_id: String, position: Int => interest: String}",
    );
    run(
        &db,
        "?[person_id, position, interest] <- [['person:ada', 0, 'mathematics'], ['person:ada', 1, 'poetry']] :put person_interest {person_id, position => interest}",
    );
    let interests = run(
        &db,
        "?[position, interest] := *person_interest{person_id: 'person:ada', position, interest} :order position",
    );
    assert_eq!(
        interests["rows"],
        serde_json::json!([[0, "mathematics"], [1, "poetry"]])
    );

    run(
        &db,
        ":create tree_node {node_id: String => kind: String, value: Any?}",
    );
    run(
        &db,
        ":create tree_edge {parent_id: String, child_id: String => key: String?, position: Int?}",
    );
    run(
        &db,
        "?[node_id, kind, value] <- [['person:ada', 'object', null], ['person:ada:name', 'string', 'Ada Lovelace'], ['person:ada:interests', 'array', null], ['person:ada:interest:0', 'string', 'mathematics'], ['person:ada:interest:1', 'string', 'poetry']] :put tree_node {node_id => kind, value}",
    );
    run(
        &db,
        "?[parent_id, child_id, key, position] <- [['person:ada', 'person:ada:name', 'name', null], ['person:ada', 'person:ada:interests', 'interests', null], ['person:ada:interests', 'person:ada:interest:0', null, 0], ['person:ada:interests', 'person:ada:interest:1', null, 1]] :put tree_edge {parent_id, child_id => key, position}",
    );
    let descendants = run(
        &db,
        "descendant[root, child] := *tree_edge{parent_id: root, child_id: child}\ndescendant[root, child] := descendant[root, middle], *tree_edge{parent_id: middle, child_id: child}\n?[child_id, kind, value] := descendant['person:ada', child_id], *tree_node{node_id: child_id, kind, value}\n:order child_id",
    );
    assert_eq!(descendants["rows"].as_array().unwrap().len(), 4);
}
