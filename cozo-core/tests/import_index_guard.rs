/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Bulk `import_relations` into a relation carrying an HNSW/FTS/LSH index warns
//! (those indices are not maintained on the bulk path) but MUST NOT fail
//! (mnestic fork, 0.10.5 hardening).
//!
//! This is a non-breaking guard on purpose: consumers legitimately import a
//! snapshot into indexed relations and rebuild the indices afterward (e.g.
//! mindgraph's `Graph::import`, which restores FTS-indexed relations). Turning
//! the warning into a hard error would break that flow, so this test pins that
//! the import still succeeds.

use cozo::{DataValue, DbInstance, NamedRows, ScriptMutability};
use std::collections::BTreeMap;

fn mutable(db: &DbInstance, script: &str) {
    db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
        .unwrap();
}

#[test]
fn import_into_fts_indexed_relation_warns_but_succeeds() {
    let db = DbInstance::new("mem", "", Default::default()).unwrap();
    mutable(&db, ":create doc { id: Int => body: String }");
    mutable(
        &db,
        "::fts create doc:body_fts { extractor: body, tokenizer: Simple, filters: [Lowercase] }",
    );

    let mut data = BTreeMap::new();
    data.insert(
        "doc".to_string(),
        NamedRows::new(
            vec!["id".to_string(), "body".to_string()],
            vec![
                vec![DataValue::from(1i64), DataValue::from("hello world")],
                vec![DataValue::from(2i64), DataValue::from("goodbye world")],
            ],
        ),
    );

    // The load itself must succeed despite the unmaintained FTS index (a warning
    // is emitted; the base rows land).
    db.import_relations(data)
        .expect("bulk import into an FTS-indexed relation should warn, not fail");

    let rows = db
        .run_script(
            "?[id, body] := *doc[id, body]",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .unwrap();
    assert_eq!(rows.rows.len(), 2, "both base rows should be present");
}
