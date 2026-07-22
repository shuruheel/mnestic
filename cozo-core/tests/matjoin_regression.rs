/*
 * Copyright 2026, Shan Rizvi (mnestic fork).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Regression test for the `stored_mat_join` symbol-mapping bug (mnestic fork #3).
//!
//! When a Datalog rule uses binding-first filtering on a non-PK column that has a
//! secondary index, AND the rule needs columns outside the index, upstream Cozo
//! 0.7.6 silently returned 0 rows because the planner joined the index uid symbol
//! against the wrong base-relation column. This test pins the correct behavior.
//!
//! Uses a persistent (SQLite) backend on purpose: the `mem` backend uses a
//! separate `mem_mat_join` operator and does not exercise the buggy
//! `stored_mat_join` path.

use cozo::{DataValue, DbInstance, ScriptMutability};
use std::collections::BTreeMap;

#[test]
fn binding_first_nonpk_indexed_lookup_returns_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mnestic_matjoin.db");
    let db = DbInstance::new("sqlite", path.to_str().unwrap(), Default::default()).unwrap();

    let run_mut = |script: &str| {
        db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
            .unwrap()
    };

    run_mut(":create things { uid: String => kind: String, label: String }");
    run_mut("::index create things:kind_idx {kind}");
    run_mut(
        r#"?[uid, kind, label] <- [
            ["u1", "A", "first"], ["u2", "A", "second"],
            ["u3", "B", "third"], ["u4", "B", "fourth"], ["u5", "B", "fifth"]
        ] :put things { uid => kind, label }"#,
    );

    let mut p = BTreeMap::new();
    p.insert("k".into(), DataValue::Str("A".into()));

    // Control: positional + post-filter (full scan) — always worked.
    let a = db
        .run_script(
            "?[uid, label] := *things[uid, kind, label], kind == $k",
            p.clone(),
            ScriptMutability::Immutable,
        )
        .unwrap();
    assert_eq!(a.rows.len(), 2, "control (post-filter) should match 2 rows");

    // Regression: binding-first with a needed column outside the index.
    let b = db
        .run_script(
            "?[uid, label] := kind = $k, *things{uid, kind, label}",
            p.clone(),
            ScriptMutability::Immutable,
        )
        .unwrap();
    assert_eq!(
        b.rows.len(),
        2,
        "binding-first non-PK indexed lookup should match 2 rows (was silently 0 upstream)"
    );

    // Both forms must agree.
    let mut a_sorted = a.rows.clone();
    let mut b_sorted = b.rows.clone();
    a_sorted.sort();
    b_sorted.sort();
    assert_eq!(
        a_sorted, b_sorted,
        "both query forms must return the same rows"
    );
}
